//! The usage meters (plan §3 "Task view": per-turn and cumulative cost and context %).
//!
//! Data comes from the `usage` state document (turn-event.md §4.6):
//! `{context_used, context_size, cost: {amount, currency} | null, turn_cost?}`. It carries no
//! token counters beyond `context_used`, so tokens are derived: [`UsageTracker`] watches
//! `context_used` and counts the growth since the turn began (per turn) and the sum of all growth
//! (cumulative; a drop, e.g. after compaction, is not subtracted).
//!
//! Hookup: keep a `UsageTracker`, call `tracker.observe(&thread)` after every
//! `ThreadView::apply` and draw `render_usage(&model)` in the task header or footer. Strings live
//! in `locales/en/task.ftl`.

use client_core::ThreadView;
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::progress::Progress;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use serde_json::Value;

use super::plan::{tr, tr_args};

/// Element id of the usage widget.
pub const USAGE_ID: &str = "usage-widget";
/// Element id of the per-turn tokens line.
pub const TURN_TOKENS_ID: &str = "usage-widget-turn-tokens";
/// Element id of the cumulative tokens line.
pub const TOTAL_TOKENS_ID: &str = "usage-widget-total-tokens";
/// Element id of the per-turn cost line.
pub const TURN_COST_ID: &str = "usage-widget-turn-cost";
/// Element id of the cumulative cost line.
pub const TOTAL_COST_ID: &str = "usage-widget-total-cost";
/// Element id of the context line (percent known).
pub const CONTEXT_ID: &str = "usage-widget-context";
/// Element id of the context line when the window size is unknown.
pub const CONTEXT_UNKNOWN_ID: &str = "usage-widget-context-unknown";

/// An amount of money.
#[derive(Debug, Clone, PartialEq)]
pub struct Money {
    pub amount: f64,
    pub currency: String,
}

impl Money {
    fn from_value(value: &Value) -> Option<Self> {
        Some(Self {
            amount: value.get("amount")?.as_f64()?,
            currency: value.get("currency")?.as_str()?.to_owned(),
        })
    }

    /// `0.042 USD`: at least two decimals, at most four, no trailing zeros beyond two.
    pub fn display(&self) -> String {
        let mut amount = format!("{:.4}", self.amount);
        while amount.ends_with('0') && amount.split_once('.').is_some_and(|(_, d)| d.len() > 2) {
            amount.pop();
        }
        tr_args(
            "usage-cost-value",
            &[("amount", &amount), ("currency", &self.currency)],
        )
    }
}

/// The `usage` state document, typed. Every field is optional: a missing or malformed field is
/// "unknown", never a guess.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageSnapshot {
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
    pub cost: Option<Money>,
    pub turn_cost: Option<Money>,
}

impl UsageSnapshot {
    pub fn from_state(state: Option<&Value>) -> Self {
        let Some(state) = state else {
            return Self::default();
        };
        Self {
            context_used: state.get("context_used").and_then(Value::as_u64),
            context_size: state.get("context_size").and_then(Value::as_u64),
            cost: state.get("cost").and_then(Money::from_value),
            turn_cost: state.get("turn_cost").and_then(Money::from_value),
        }
    }
}

/// What the usage widget shows.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct UsageModel {
    pub turn_tokens: Option<u64>,
    pub total_tokens: Option<u64>,
    pub turn_cost: Option<Money>,
    pub total_cost: Option<Money>,
    pub context_used: Option<u64>,
    pub context_size: Option<u64>,
}

impl UsageModel {
    /// A stateless reading of `thread`: totals only (context tokens as the total; the document's
    /// own `turn_cost` as the turn's cost). Use [`UsageTracker`] for per-turn tokens.
    pub fn from_thread(thread: &ThreadView) -> Self {
        let snap = UsageSnapshot::from_state(thread.state("usage"));
        Self {
            turn_tokens: None,
            total_tokens: snap.context_used,
            turn_cost: snap.turn_cost,
            total_cost: snap.cost,
            context_used: snap.context_used,
            context_size: snap.context_size,
        }
    }

    /// Context used as a whole percent of the window, capped at 100. `None` (hidden) when the
    /// window size is unknown or zero, or the used count is unknown.
    pub fn context_percent(&self) -> Option<u8> {
        let (used, size) = (self.context_used?, self.context_size?);
        if size == 0 {
            return None;
        }
        let percent = (u128::from(used) * 100 + u128::from(size) / 2) / u128::from(size);
        Some(percent.min(100) as u8)
    }

    /// `true` when there is nothing to show at all.
    pub fn is_empty(&self) -> bool {
        self.total_tokens.is_none()
            && self.total_cost.is_none()
            && self.turn_cost.is_none()
            && self.context_used.is_none()
            && self.context_size.is_none()
    }
}

/// `512`, `48.2k`, `1.3M`.
pub fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        tokens.to_string()
    } else if tokens < 1_000_000 {
        tr_args("usage-tokens-k", &[("value", &one_decimal(tokens, 1_000))])
    } else {
        tr_args(
            "usage-tokens-m",
            &[("value", &one_decimal(tokens, 1_000_000))],
        )
    }
}

fn one_decimal(value: u64, unit: u64) -> String {
    let tenths = (u128::from(value) * 10 + u128::from(unit) / 2) / u128::from(unit);
    if tenths % 10 == 0 {
        (tenths / 10).to_string()
    } else {
        format!("{}.{}", tenths / 10, tenths % 10)
    }
}

/// Folds successive `usage` states and task starts into per-turn and cumulative figures.
#[derive(Debug, Clone, Default)]
pub struct UsageTracker {
    tasks_seen: usize,
    last_used: Option<u64>,
    last_cost: Option<Money>,
    turn_start_used: u64,
    turn_start_cost: Option<Money>,
    cumulative_tokens: u64,
}

impl UsageTracker {
    pub fn new() -> Self {
        Self::default()
    }

    /// Reads `thread` after an `apply` and returns the figures. Safe to call repeatedly with an
    /// unchanged thread. A task not seen before opens a new turn at the last known usage.
    pub fn observe(&mut self, thread: &ThreadView) -> UsageModel {
        let snap = UsageSnapshot::from_state(thread.state("usage"));
        let tasks = thread.tasks().len();
        if tasks > self.tasks_seen {
            self.tasks_seen = tasks;
            self.turn_start_used = self.last_used.unwrap_or(0);
            self.turn_start_cost = self.last_cost.clone();
        }
        if let Some(used) = snap.context_used {
            self.cumulative_tokens += match self.last_used {
                Some(prev) => used.saturating_sub(prev),
                None => used,
            };
            self.last_used = Some(used);
        }
        if snap.cost.is_some() {
            self.last_cost = snap.cost.clone();
        }
        let turn_cost = snap.turn_cost.clone().or_else(|| {
            let total = snap.cost.as_ref()?;
            match &self.turn_start_cost {
                None => Some(total.clone()),
                Some(start) if start.currency == total.currency => Some(Money {
                    amount: (total.amount - start.amount).max(0.0),
                    currency: total.currency.clone(),
                }),
                Some(_) => None,
            }
        });
        UsageModel {
            turn_tokens: snap
                .context_used
                .map(|used| used.saturating_sub(self.turn_start_used)),
            total_tokens: snap.context_used.map(|_| self.cumulative_tokens),
            turn_cost,
            total_cost: snap.cost,
            context_used: snap.context_used,
            context_size: snap.context_size,
        }
    }
}

fn line(id: &'static str, label: String) -> impl IntoElement {
    div()
        .id(id)
        .test_support()
        .aria_label(label.clone())
        .text_sm()
        .child(label)
}

/// Draws the meters. Stateless. Rows with no data are left out; an unknown window shows an
/// explicit "Context window unknown" line instead of a percent.
pub fn render_usage(model: &UsageModel) -> impl IntoElement {
    let model = model.clone();
    let percent = model.context_percent();
    let context_label = percent.map(|percent| {
        tr_args(
            "usage-context",
            &[
                ("percent", &percent.to_string()),
                ("size", &format_tokens(model.context_size.unwrap_or(0))),
            ],
        )
    });
    let tokens_line = |id, key, tokens: Option<u64>| {
        tokens.map(|tokens| line(id, tr_args(key, &[("tokens", &format_tokens(tokens))])))
    };
    let cost_line = |id, key, cost: &Option<Money>| {
        cost.as_ref()
            .map(|cost| line(id, tr_args(key, &[("cost", &cost.display())])))
    };
    v_flex()
        .id(USAGE_ID)
        .role(Role::Group)
        .test_support()
        .aria_label(tr("usage-region-a11y"))
        .w_full()
        .gap_1()
        .p_2()
        .child(
            div()
                .text_sm()
                .font_weight(FontWeight::SEMIBOLD)
                .child(tr("usage-title")),
        )
        .children(tokens_line(
            TURN_TOKENS_ID,
            "usage-turn-tokens",
            model.turn_tokens,
        ))
        .children(tokens_line(
            TOTAL_TOKENS_ID,
            "usage-total-tokens",
            model.total_tokens,
        ))
        .children(cost_line(TURN_COST_ID, "usage-turn-cost", &model.turn_cost))
        .children(cost_line(
            TOTAL_COST_ID,
            "usage-total-cost",
            &model.total_cost,
        ))
        .when(model.total_cost.is_none() && !model.is_empty(), |el| {
            el.child(line("usage-widget-cost-unknown", tr("usage-cost-unknown")))
        })
        .when_some(context_label.zip(percent), |el, (label, percent)| {
            el.child(
                h_flex()
                    .id(CONTEXT_ID)
                    .test_support()
                    .aria_label(tr_args(
                        "usage-context-a11y",
                        &[("percent", &percent.to_string())],
                    ))
                    .gap_2()
                    .child(div().text_sm().child(label))
                    .child(
                        div().id("usage-widget-context-bar").w(px(80.)).child(
                            Progress::new("usage-context-progress").value(f32::from(percent)),
                        ),
                    ),
            )
        })
        .when(percent.is_none() && !model.is_empty(), |el| {
            el.child(line(CONTEXT_UNKNOWN_ID, tr("usage-context-unknown")))
        })
}
