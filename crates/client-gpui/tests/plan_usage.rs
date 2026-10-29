//! M3-009: the plan view and the usage meters. Models natively (successive TurnEvents through
//! `client-core`), widgets headlessly, strings against `locales/en/task.ftl`.

use std::collections::BTreeSet;

use client_core::ThreadView;
use client_gpui::shell::i18n::Catalog;
use client_gpui::task::plan::{
    EN_FTL, PLAN_EMPTY_ID, PLAN_ID, PLAN_ROW_ID, PLAN_SUMMARY_ID, PlanModel, render_plan,
};
use client_gpui::task::usage::{
    CONTEXT_ID, CONTEXT_UNKNOWN_ID, Money, TOTAL_COST_ID, TOTAL_TOKENS_ID, TURN_COST_ID,
    TURN_TOKENS_ID, USAGE_ID, UsageModel, UsageSnapshot, UsageTracker, format_tokens, render_usage,
};
use gpui_kit::test::{TestSupportExt as _, TestWindowExt as _};
use gpui_kit::{
    AppContext as _, Bounds, Context, InteractiveElement as _, IntoElement, ParentElement as _,
    Point, Render, Styled as _, TestAppContext, Window, WindowBounds, WindowOptions, div, px, size,
};
use protocol::turn_event::{Envelope, PlanPriority, PlanStatus};
use serde_json::{Value, json};

// ---- fixtures --------------------------------------------------------------------------------

fn envelope(seq: u64, task: Option<&str>, run: Option<&str>, event: Value) -> Envelope {
    serde_json::from_value(json!({
        "v": 1, "seq": seq, "thread_id": "th_1", "task_id": task, "run_id": run,
        "ts": "2026-10-27T22:00:01.000Z", "event": event,
    }))
    .expect("envelope")
}

fn start_task(thread: &mut ThreadView, seq: &mut u64, task: &str, run: &str) {
    let user = json!({"type": "UserMessage", "message_id": format!("m_{task}"),
        "parts": [{"type": "text", "text": "go"}],
        "author": {"kind": "user", "id": "usr_1", "name": "Jacob", "device_id": "d"}});
    let started = json!({"type": "RunStarted", "reason": "prompt", "agent_id": "a",
        "machine_id": "m"});
    for (task_run, event) in [(None, user), (Some(run), started)] {
        *seq += 1;
        thread
            .apply(&envelope(*seq, Some(task), task_run, event))
            .expect("apply");
    }
}

fn finish_task(thread: &mut ThreadView, seq: &mut u64, task: &str, run: &str) {
    *seq += 1;
    let event = json!({"type": "RunFinished",
        "outcome": {"type": "success", "stop_reason": "end_turn"}});
    thread
        .apply(&envelope(*seq, Some(task), Some(run), event))
        .expect("apply");
}

fn plan_snapshot(thread: &mut ThreadView, seq: &mut u64, task: &str, run: &str, entries: Value) {
    *seq += 1;
    let event = json!({"type": "Activity", "activity_id": "plan", "activity_type": "plan",
        "mode": "snapshot", "content": {"entries": entries}});
    thread
        .apply(&envelope(*seq, Some(task), Some(run), event))
        .expect("apply plan");
}

fn entry(content: &str, priority: &str, status: &str) -> Value {
    json!({"content": content, "priority": priority, "status": status})
}

fn usage_snapshot(thread: &mut ThreadView, seq: &mut u64, doc: Value) {
    *seq += 1;
    let event = json!({"type": "StateSnapshot", "scope": "usage", "snapshot": doc});
    thread
        .apply(&envelope(*seq, None, None, event))
        .expect("apply usage snapshot");
}

fn usage_delta(thread: &mut ThreadView, seq: &mut u64, patch: Value) {
    *seq += 1;
    let event = json!({"type": "StateDelta", "scope": "usage", "patch": patch});
    thread
        .apply(&envelope(*seq, None, None, event))
        .expect("apply usage delta");
}

fn replace(path: &str, value: Value) -> Value {
    json!({"op": "replace", "path": path, "value": value})
}

fn rows(model: &PlanModel) -> Vec<(String, PlanStatus, PlanPriority)> {
    model
        .rows
        .iter()
        .map(|row| (row.content.clone(), row.status, row.priority))
        .collect()
}

// ---- plan model ------------------------------------------------------------------------------

#[test]
fn the_plan_updates_as_successive_plan_activities_arrive() {
    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    assert!(PlanModel::from_thread(&thread).is_empty());
    start_task(&mut thread, &mut seq, "tk_1", "run_1");
    assert!(PlanModel::from_thread(&thread).is_empty());

    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_1",
        "run_1",
        json!([
            entry("Add route", "high", "in_progress"),
            entry("Add test", "medium", "pending")
        ]),
    );
    let first = PlanModel::from_thread(&thread);
    assert_eq!(
        rows(&first),
        vec![
            (
                "Add route".to_owned(),
                PlanStatus::InProgress,
                PlanPriority::High
            ),
            (
                "Add test".to_owned(),
                PlanStatus::Pending,
                PlanPriority::Medium
            ),
        ]
    );
    assert_eq!(first.summary(), "0 of 2 done");
    assert_eq!(first.rows[0].status_label, "In progress");
    assert_eq!(first.rows[0].priority_label, "High priority");
    assert_eq!(first.rows[0].a11y, "Add route: In progress, High priority");

    // The agent sends the full list again: it replaces the first one, with a new entry.
    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_1",
        "run_1",
        json!([
            entry("Add route", "high", "completed"),
            entry("Add test", "medium", "in_progress"),
            entry("Update docs", "low", "pending")
        ]),
    );
    let second = PlanModel::from_thread(&thread);
    assert_eq!(second.total(), 3);
    assert_eq!(second.completed(), 1);
    assert_eq!(second.summary(), "1 of 3 done");
    assert_eq!(second.rows[0].status, PlanStatus::Completed);
    assert_eq!(second.rows[1].status, PlanStatus::InProgress);
    assert_eq!(second.rows[2].priority, PlanPriority::Low);
    assert_ne!(first, second);

    // A JSON-patch plan activity updates one entry in place.
    seq += 1;
    let patch = json!({"type": "Activity", "activity_id": "plan", "activity_type": "plan",
        "mode": "patch", "patch": [replace("/entries/1/status", json!("completed"))]});
    thread
        .apply(&envelope(seq, Some("tk_1"), Some("run_1"), patch))
        .expect("patch");
    let third = PlanModel::from_thread(&thread);
    assert_eq!(third.completed(), 2);
    assert_eq!(third.rows.len(), 3);
}

#[test]
fn the_latest_task_with_a_plan_wins() {
    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    start_task(&mut thread, &mut seq, "tk_1", "run_1");
    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_1",
        "run_1",
        json!([entry("Old step", "low", "completed")]),
    );
    finish_task(&mut thread, &mut seq, "tk_1", "run_1");
    start_task(&mut thread, &mut seq, "tk_2", "run_2");
    // tk_2 has no plan yet: the plan of tk_1 is still the latest.
    assert_eq!(PlanModel::from_thread(&thread).rows[0].content, "Old step");
    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_2",
        "run_2",
        json!([entry("New step", "high", "pending")]),
    );
    let model = PlanModel::from_thread(&thread);
    assert_eq!(model.total(), 1);
    assert_eq!(model.rows[0].content, "New step");
}

// ---- usage model -----------------------------------------------------------------------------

#[test]
fn context_percent_is_known_only_with_a_window_size() {
    let model = |used, size| UsageModel {
        context_used: used,
        context_size: size,
        ..UsageModel::default()
    };
    assert_eq!(
        model(Some(48_211), Some(200_000)).context_percent(),
        Some(24)
    );
    assert_eq!(model(Some(0), Some(200_000)).context_percent(), Some(0));
    assert_eq!(
        model(Some(300_000), Some(200_000)).context_percent(),
        Some(100)
    );
    assert_eq!(model(Some(10), None).context_percent(), None);
    assert_eq!(model(Some(10), Some(0)).context_percent(), None);
    assert_eq!(model(None, Some(200_000)).context_percent(), None);
}

#[test]
fn snapshot_parsing_treats_missing_and_malformed_fields_as_unknown() {
    let snap = UsageSnapshot::from_state(Some(&json!({
        "context_used": 5, "context_size": "big", "cost": null,
        "turn_cost": {"amount": 0.5, "currency": "EUR"}
    })));
    assert_eq!(snap.context_used, Some(5));
    assert_eq!(snap.context_size, None);
    assert_eq!(snap.cost, None);
    assert_eq!(
        snap.turn_cost,
        Some(Money {
            amount: 0.5,
            currency: "EUR".into()
        })
    );
    assert_eq!(UsageSnapshot::from_state(None), UsageSnapshot::default());
}

#[test]
fn token_and_money_formatting() {
    assert_eq!(format_tokens(512), "512");
    assert_eq!(format_tokens(48_211), "48.2k");
    assert_eq!(format_tokens(200_000), "200k");
    assert_eq!(format_tokens(1_250_000), "1.3M");
    let money = |amount| Money {
        amount,
        currency: "USD".into(),
    };
    assert_eq!(money(0.042).display(), "0.042 USD");
    assert_eq!(money(1.0).display(), "1.00 USD");
    assert_eq!(money(0.05).display(), "0.05 USD");
}

#[test]
fn the_tracker_reports_per_turn_and_cumulative_tokens_and_cost() {
    let mut thread = ThreadView::new("th_1");
    let mut tracker = UsageTracker::new();
    let mut seq = 0;
    assert!(tracker.observe(&thread).is_empty());

    start_task(&mut thread, &mut seq, "tk_1", "run_1");
    usage_snapshot(
        &mut thread,
        &mut seq,
        json!({"context_used": 10_000, "context_size": 200_000,
               "cost": {"amount": 0.01, "currency": "USD"}}),
    );
    let one = tracker.observe(&thread);
    assert_eq!(one.turn_tokens, Some(10_000));
    assert_eq!(one.total_tokens, Some(10_000));
    assert_eq!(one.total_cost.as_ref().map(|m| m.amount), Some(0.01));
    assert_eq!(one.turn_cost.as_ref().map(|m| m.amount), Some(0.01));
    assert_eq!(one.context_percent(), Some(5));

    usage_delta(
        &mut thread,
        &mut seq,
        json!([
            replace("/context_used", json!(30_000)),
            replace("/cost", json!({"amount": 0.03, "currency": "USD"}))
        ]),
    );
    let two = tracker.observe(&thread);
    assert_eq!(two.turn_tokens, Some(30_000));
    assert_eq!(two.total_tokens, Some(30_000));
    assert_eq!(two.context_percent(), Some(15));
    // Observing again with no change adds nothing.
    assert_eq!(tracker.observe(&thread), two);

    finish_task(&mut thread, &mut seq, "tk_1", "run_1");
    start_task(&mut thread, &mut seq, "tk_2", "run_2");
    usage_delta(
        &mut thread,
        &mut seq,
        json!([
            replace("/context_used", json!(35_000)),
            replace("/cost", json!({"amount": 0.045, "currency": "USD"}))
        ]),
    );
    let three = tracker.observe(&thread);
    assert_eq!(
        three.turn_tokens,
        Some(5_000),
        "only the second turn's growth"
    );
    assert_eq!(three.total_tokens, Some(35_000));
    let turn_cost = three.turn_cost.expect("turn cost");
    assert!((turn_cost.amount - 0.015).abs() < 1e-9, "{turn_cost:?}");
    assert_eq!(three.total_cost.as_ref().map(|m| m.amount), Some(0.045));

    // The document's own turn_cost wins over the derived one.
    usage_delta(
        &mut thread,
        &mut seq,
        json!([{"op": "add", "path": "/turn_cost",
                "value": {"amount": 0.02, "currency": "USD"}}]),
    );
    assert_eq!(
        tracker.observe(&thread).turn_cost.map(|m| m.amount),
        Some(0.02)
    );

    // Compaction shrinks the context; cumulative tokens never go down.
    usage_delta(
        &mut thread,
        &mut seq,
        json!([replace("/context_used", json!(8_000))]),
    );
    let shrunk = tracker.observe(&thread);
    assert_eq!(shrunk.total_tokens, Some(35_000));
    assert_eq!(shrunk.turn_tokens, Some(0));
}

#[test]
fn unknown_cost_and_window_stay_unknown() {
    let mut thread = ThreadView::new("th_1");
    let mut tracker = UsageTracker::new();
    let mut seq = 0;
    usage_snapshot(
        &mut thread,
        &mut seq,
        json!({"context_used": 1_000, "cost": null}),
    );
    let model = tracker.observe(&thread);
    assert_eq!(model.total_cost, None);
    assert_eq!(model.turn_cost, None);
    assert_eq!(model.context_size, None);
    assert_eq!(model.context_percent(), None);
    assert_eq!(model.total_tokens, Some(1_000));
    assert_eq!(UsageModel::from_thread(&thread).total_tokens, Some(1_000));
}

// ---- headless widgets ------------------------------------------------------------------------

struct Host {
    plan: PlanModel,
    usage: UsageModel,
}

impl Render for Host {
    fn render(&mut self, _: &mut Window, _: &mut Context<Self>) -> impl IntoElement {
        div()
            .id("host")
            .test_support()
            .size_full()
            .child(render_plan(&self.plan))
            .child(render_usage(&self.usage))
    }
}

fn window_options() -> WindowOptions {
    WindowOptions {
        window_bounds: Some(WindowBounds::Windowed(Bounds {
            origin: Point::default(),
            size: size(px(700.), px(700.)),
        })),
        ..Default::default()
    }
}

fn labels(window: &mut Window, id: &'static str) -> Vec<String> {
    window
        .find_all(id)
        .iter()
        .map(|e| e.label().unwrap_or_default().to_owned())
        .collect()
}

#[gpui_kit::test]
fn the_plan_widget_redraws_rows_when_the_plan_changes(cx: &mut TestAppContext) {
    cx.update(gpui_kit::init);
    let (window, host) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |_, cx| {
            cx.new(|_| Host {
                plan: PlanModel::default(),
                usage: UsageModel::default(),
            })
        })
        .expect("window")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(PLAN_ID).visible());
        assert_eq!(window.find(PLAN_EMPTY_ID).label(), Some("No plan yet"));
        assert!(window.find_all(PLAN_ROW_ID).is_empty());
    })
    .expect("window is open");

    let mut thread = ThreadView::new("th_1");
    let mut seq = 0;
    start_task(&mut thread, &mut seq, "tk_1", "run_1");
    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_1",
        "run_1",
        json!([
            entry("Add route", "high", "in_progress"),
            entry("Add test", "medium", "pending")
        ]),
    );
    cx.update(|cx| {
        host.update(cx, |host, cx| {
            host.plan = PlanModel::from_thread(&thread);
            cx.notify();
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all(PLAN_EMPTY_ID).is_empty());
        assert_eq!(
            labels(window, PLAN_ROW_ID),
            vec![
                "Add route: In progress, High priority",
                "Add test: Pending, Medium priority"
            ]
        );
        assert_eq!(window.find(PLAN_SUMMARY_ID).label(), Some("0 of 2 done"));
    })
    .expect("window is open");

    plan_snapshot(
        &mut thread,
        &mut seq,
        "tk_1",
        "run_1",
        json!([
            entry("Add route", "high", "completed"),
            entry("Add test", "medium", "in_progress"),
            entry("Update docs", "low", "pending")
        ]),
    );
    cx.update(|cx| {
        host.update(cx, |host, cx| {
            host.plan = PlanModel::from_thread(&thread);
            cx.notify();
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            labels(window, PLAN_ROW_ID),
            vec![
                "Add route: Completed, High priority",
                "Add test: In progress, Medium priority",
                "Update docs: Pending, Low priority"
            ]
        );
        assert_eq!(window.find(PLAN_SUMMARY_ID).label(), Some("1 of 3 done"));
    })
    .expect("window is open");
}

#[gpui_kit::test]
fn the_usage_widget_shows_figures_and_hides_the_percent_when_the_window_is_unknown(
    cx: &mut TestAppContext,
) {
    cx.update(gpui_kit::init);
    let (window, host) = cx.update(|cx| {
        gpui_kit::open_window(window_options(), cx, |_, cx| {
            cx.new(|_| Host {
                plan: PlanModel::default(),
                usage: UsageModel::default(),
            })
        })
        .expect("window")
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find(USAGE_ID).visible());
        // Nothing reported yet: no figures and no "unknown" lines either.
        for id in [TURN_TOKENS_ID, TOTAL_TOKENS_ID, TURN_COST_ID, TOTAL_COST_ID] {
            assert!(window.find_all(id).is_empty(), "{id}");
        }
        assert!(window.find_all(CONTEXT_ID).is_empty());
        assert!(window.find_all(CONTEXT_UNKNOWN_ID).is_empty());
    })
    .expect("window is open");

    let money = |amount| Money {
        amount,
        currency: "USD".into(),
    };
    let known = UsageModel {
        turn_tokens: Some(5_000),
        total_tokens: Some(48_211),
        turn_cost: Some(money(0.015)),
        total_cost: Some(money(0.042)),
        context_used: Some(48_211),
        context_size: Some(200_000),
    };
    cx.update(|cx| {
        host.update(cx, |host, cx| {
            host.usage = known.clone();
            cx.notify();
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find(TURN_TOKENS_ID).label(),
            Some("This turn: 5k tokens")
        );
        assert_eq!(
            window.find(TOTAL_TOKENS_ID).label(),
            Some("Total: 48.2k tokens")
        );
        assert_eq!(
            window.find(TURN_COST_ID).label(),
            Some("This turn: 0.015 USD")
        );
        assert_eq!(window.find(TOTAL_COST_ID).label(), Some("Total: 0.042 USD"));
        assert_eq!(
            window.find(CONTEXT_ID).label(),
            Some("Context 24 percent used")
        );
        assert!(window.find_all(CONTEXT_UNKNOWN_ID).is_empty());
    })
    .expect("window is open");

    cx.update(|cx| {
        host.update(cx, |host, cx| {
            host.usage = UsageModel {
                context_size: None,
                total_cost: None,
                turn_cost: None,
                ..known.clone()
            };
            cx.notify();
        })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all(CONTEXT_ID).is_empty());
        assert_eq!(
            window.find(CONTEXT_UNKNOWN_ID).label(),
            Some("Context window unknown")
        );
        assert!(window.find_all(TOTAL_COST_ID).is_empty());
        assert_eq!(
            window.find("usage-widget-cost-unknown").label(),
            Some("Cost unknown")
        );
    })
    .expect("window is open");
}

// ---- catalog ---------------------------------------------------------------------------------

#[test]
fn every_string_id_in_the_code_is_in_the_catalog_and_every_catalog_id_is_used() {
    let catalog = Catalog::from_source("en", EN_FTL).expect("task.ftl parses");
    let sources = [
        include_str!("../src/task/plan.rs"),
        include_str!("../src/task/usage.rs"),
    ];
    let mut used = BTreeSet::new();
    for source in sources {
        let mut rest = source;
        while let Some(start) = rest.find('"') {
            let tail = &rest[start + 1..];
            let end = tail.find('"').expect("closing quote");
            let literal = &tail[..end];
            if (literal.starts_with("plan-") || literal.starts_with("usage-"))
                && literal
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
                && !literal.contains("-widget")
                && literal != "usage-context-progress"
            {
                used.insert(literal.to_owned());
            }
            rest = &tail[end + 1..];
        }
    }
    assert!(used.len() > 15, "scanner found only {used:?}");
    for id in &used {
        assert!(catalog.has(id), "string id {id} is not in task.ftl");
    }
    for id in Catalog::ids(EN_FTL) {
        assert!(
            used.contains(&id),
            "task.ftl id {id} is not used by the code"
        );
    }
}
