//! The plan view (plan §3 "Task view": plan). [`PlanModel`] is plain data built from the latest
//! `Activity{plan}` in a thread (`client-core` already merges snapshots, so each plan activity
//! replaces the whole list); [`render_plan`] draws it. Strings live in `locales/en/task.ftl`.
//!
//! Hookup (for the transcript/task shell): keep a `PlanModel`, call
//! [`PlanModel::from_thread`] after every `ThreadView::apply`, and place `render_plan(&model)`
//! in the task pane's side or header area.

use std::sync::OnceLock;

use client_core::ThreadView;
use client_core::view::Item;
use gpui_kit::TestSupportExt as _;
use gpui_kit::component::{h_flex, v_flex};
use gpui_kit::prelude::*;
use gpui_kit::*;
use protocol::turn_event::{ActivityType, PlanEntry, PlanPriority, PlanStatus};

use crate::shell::i18n::Catalog;

/// The English strings of the plan view and usage meter (`locales/en/task.ftl`).
pub const EN_FTL: &str = include_str!("../../locales/en/task.ftl");

pub(crate) fn catalog() -> &'static Catalog {
    static CATALOG: OnceLock<Catalog> = OnceLock::new();
    CATALOG.get_or_init(|| Catalog::from_source("en", EN_FTL).expect("locales/en/task.ftl parses"))
}

pub(crate) fn tr(id: &str) -> String {
    catalog().tr(id, &[])
}

pub(crate) fn tr_args(id: &str, args: &[(&str, &str)]) -> String {
    catalog().tr(id, args)
}

/// Element id of the plan widget.
pub const PLAN_ID: &str = "plan-widget";
/// Element id carried by every plan row.
pub const PLAN_ROW_ID: &str = "plan-widget-row";
/// Element id of the "n of m done" line.
pub const PLAN_SUMMARY_ID: &str = "plan-widget-summary";
/// Element id of the empty state.
pub const PLAN_EMPTY_ID: &str = "plan-widget-empty";

/// One plan entry, ready to draw.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PlanRow {
    pub content: String,
    pub status: PlanStatus,
    pub priority: PlanPriority,
    pub status_label: String,
    pub priority_label: String,
    /// The screen-reader label: `Add route: In progress, High priority`.
    pub a11y: String,
}

impl PlanRow {
    pub fn new(entry: &PlanEntry) -> Self {
        let status_label = tr(match entry.status {
            PlanStatus::Pending => "plan-status-pending",
            PlanStatus::InProgress => "plan-status-in-progress",
            PlanStatus::Completed => "plan-status-completed",
        });
        let priority_label = tr(match entry.priority {
            PlanPriority::High => "plan-priority-high",
            PlanPriority::Medium => "plan-priority-medium",
            PlanPriority::Low => "plan-priority-low",
        });
        let a11y = tr_args(
            "plan-row-a11y",
            &[
                ("content", &entry.content),
                ("status", &status_label),
                ("priority", &priority_label),
            ],
        );
        Self {
            content: entry.content.clone(),
            status: entry.status,
            priority: entry.priority,
            status_label,
            priority_label,
            a11y,
        }
    }

    /// The status mark drawn before the text.
    pub fn marker(&self) -> &'static str {
        match self.status {
            PlanStatus::Pending => "\u{25CB}",
            PlanStatus::InProgress => "\u{25D0}",
            PlanStatus::Completed => "\u{25CF}",
        }
    }
}

/// The plan as last reported.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct PlanModel {
    pub rows: Vec<PlanRow>,
}

impl PlanModel {
    pub fn from_entries(entries: &[PlanEntry]) -> Self {
        Self {
            rows: entries.iter().map(PlanRow::new).collect(),
        }
    }

    /// The latest plan in `thread`: the last task that has a plan activity, and in it the plan
    /// activity as the transcript holds it now. Empty when there is none.
    pub fn from_thread(thread: &ThreadView) -> Self {
        let latest = thread.tasks().iter().rev().find_map(|task| {
            task.transcript()
                .items()
                .iter()
                .rev()
                .find_map(|item| match item {
                    Item::Activity(activity) if activity.activity_type == ActivityType::Plan => {
                        activity.plan()
                    }
                    _ => None,
                })
        });
        latest.map_or_else(Self::default, |plan| Self::from_entries(&plan.entries))
    }

    pub fn is_empty(&self) -> bool {
        self.rows.is_empty()
    }

    pub fn total(&self) -> usize {
        self.rows.len()
    }

    pub fn completed(&self) -> usize {
        self.rows
            .iter()
            .filter(|row| row.status == PlanStatus::Completed)
            .count()
    }

    /// `2 of 5 done`.
    pub fn summary(&self) -> String {
        tr_args(
            "plan-summary",
            &[
                ("done", &self.completed().to_string()),
                ("total", &self.total().to_string()),
            ],
        )
    }

    fn region_label(&self) -> String {
        tr_args(
            "plan-region-a11y",
            &[
                ("done", &self.completed().to_string()),
                ("total", &self.total().to_string()),
            ],
        )
    }
}

/// Draws the plan. Stateless; entry text reaches `div().child(String)` only.
pub fn render_plan(model: &PlanModel) -> impl IntoElement {
    let model = model.clone();
    v_flex()
        .id(PLAN_ID)
        .role(Role::Group)
        .test_support()
        .aria_label(model.region_label())
        .w_full()
        .gap_1()
        .p_2()
        .child(
            h_flex()
                .gap_2()
                .child(
                    div()
                        .text_sm()
                        .font_weight(FontWeight::SEMIBOLD)
                        .child(tr("plan-title")),
                )
                .when(!model.is_empty(), |el| {
                    el.child(
                        div()
                            .id(PLAN_SUMMARY_ID)
                            .test_support()
                            .aria_label(model.summary())
                            .text_xs()
                            .child(model.summary()),
                    )
                }),
        )
        .when(model.is_empty(), |el| {
            el.child(
                div()
                    .id(PLAN_EMPTY_ID)
                    .test_support()
                    .aria_label(tr("plan-empty"))
                    .text_sm()
                    .child(tr("plan-empty")),
            )
        })
        .children(model.rows.into_iter().enumerate().map(|(ix, row)| {
            h_flex()
                .id(("plan-widget-item", ix))
                .gap_2()
                .child(div().text_sm().child(row.marker()))
                .child(
                    div()
                        .id(PLAN_ROW_ID)
                        .test_support()
                        .aria_label(row.a11y.clone())
                        .flex_1()
                        .text_sm()
                        .child(row.content.clone()),
                )
                .child(div().text_xs().child(row.priority_label.clone()))
                .child(div().text_xs().child(row.status_label.clone()))
        }))
}
