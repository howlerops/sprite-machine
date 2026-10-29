//! M3-012: the agents table, the disabled BYO action and the managed launch form. The model
//! natively (unit tests live next to it), the view headlessly.

use std::collections::BTreeSet;
use std::sync::Arc;

use client_gpui::agents::{
    AgentsView, EN_FTL, FakeManagedClient, LaunchConfig, LaunchState, MachinesResponse,
    agents_from_machines,
};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};

const HUB_BODY: &str = r#"{"machines":[
  {"machine_id":"mch_b","name":"build-box","kind":"local",
   "presence":{"state":"offline","since":"2026-10-27T22:00:00.000Z"},
   "capabilities":{"harnesses":["claude"]}},
  {"machine_id":"mch_a","name":"my-laptop","kind":"local",
   "presence":{"state":"online","since":"2026-10-27T22:00:00.000Z"},
   "capabilities":{"harnesses":["opencode","claude"],"permission_mode":"default"}}]}"#;

fn machines() -> Vec<client_gpui::agents::MachineInfo> {
    MachinesResponse::parse(HUB_BODY).unwrap().machines
}

fn open(
    cx: &mut TestAppContext,
    config: LaunchConfig,
    client: FakeManagedClient,
) -> (AnyWindowHandle, Entity<AgentsView>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(900.), px(900.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| {
                cx.new(|cx| AgentsView::new(&machines(), config, Arc::new(client), window, cx))
            },
        )
        .expect("open window")
    })
}

const KEY: LaunchConfig = LaunchConfig {
    model_key_configured: true,
};

// ---- strings ---------------------------------------------------------------------------------------

#[test]
fn every_string_id_the_agents_module_uses_is_in_the_catalog() {
    let catalog = Catalog::from_source("en", EN_FTL).expect("agents.ftl parses");
    let mut ids = BTreeSet::new();
    for source in [
        include_str!("../src/agents/view.rs"),
        include_str!("../src/agents/model.rs"),
    ] {
        for line in source.lines() {
            if line.trim_start().starts_with("//") {
                continue;
            }
            let mut rest = line;
            while let Some(start) = rest.find("\"agents-") {
                rest = &rest[start + 1..];
                if let Some(end) = rest.find('"') {
                    ids.insert(rest[..end].to_owned());
                    rest = &rest[end..];
                }
            }
        }
    }
    for kind in ["local", "managed", "byo"] {
        ids.insert(format!("agents-kind-{kind}"));
    }
    for slug in ["online", "offline", "unknown"] {
        ids.insert(format!("agents-presence-{slug}"));
    }
    for slug in ["ready", "unreachable", "unknown"] {
        ids.insert(format!("agents-health-{slug}"));
    }
    // Element ids (not strings) that share the prefix.
    for element_id in [
        "agents-header",
        "agents-table",
        "agents-byo",
        "agents-byo-reason",
        "agents-launch",
        "agents-launch-submit",
        "agents-launch-reason",
        "agents-launch-outcome",
    ] {
        ids.remove(element_id);
    }
    ids.retain(|id| !id.contains('{') && !id.ends_with('-'));
    assert!(ids.len() >= 25, "scan found only {} ids", ids.len());
    let missing: Vec<_> = ids.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from agents.ftl: {missing:?}"
    );
    let unused: Vec<_> = Catalog::ids(EN_FTL)
        .into_iter()
        .filter(|id| !ids.contains(id))
        .collect();
    assert!(unused.is_empty(), "unused ids in agents.ftl: {unused:?}");
}

// ---- headless --------------------------------------------------------------------------------------

#[gpui_kit::test]
fn the_table_lists_each_harness_with_where_it_runs_and_presence(cx: &mut TestAppContext) {
    assert_eq!(agents_from_machines(&machines()).len(), 3);
    let (window, view) = open(cx, KEY, FakeManagedClient::ok("ag_1"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("agents-row-0").label(),
            Some("claude, Local harness, runs on my-laptop, Online, health Ready")
        );
        assert_eq!(
            window.find("agents-row-1").label(),
            Some("opencode, Local harness, runs on my-laptop, Online, health Ready")
        );
        assert_eq!(
            window.find("agents-row-2").label(),
            Some("claude, Local harness, runs on build-box, Offline, health Unreachable")
        );
    })
    .expect("window");
    assert_eq!(view.read_with(cx, |v, _| v.rows().len()), 3);
}

#[gpui_kit::test]
fn the_byo_button_is_disabled_and_says_why(cx: &mut TestAppContext) {
    let (window, view) = open(cx, KEY, FakeManagedClient::ok("ag_1"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("agents-byo-reason").label(),
            Some("Bring-your-own agents need the A2A edge, which is not available yet.")
        );
    })
    .expect("window");
    assert!(!view.read_with(cx, |v, _| v.byo_enabled()));
}

#[gpui_kit::test]
fn without_a_model_key_launch_is_disabled_with_a_reason(cx: &mut TestAppContext) {
    let client = FakeManagedClient::ok("ag_1");
    let (window, view) = open(cx, LaunchConfig::default(), client.clone());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("agents-launch-reason").label(),
            Some("Add a model API key in Settings before launching a managed agent.")
        );
        window.click("agents-launch-submit", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert!(client.requests.lock().unwrap().is_empty());
    assert!(!view.read_with(cx, |v, _| v.launch_enabled()));
    assert_eq!(
        view.read_with(cx, |v, _| v.launch_state().clone()),
        LaunchState::Idle
    );
}

#[gpui_kit::test]
fn with_a_key_an_invalid_form_shows_errors_and_a_valid_one_calls_the_client(
    cx: &mut TestAppContext,
) {
    let client = FakeManagedClient::ok("ag_42");
    let (window, view) = open(cx, KEY, client.clone());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all("agents-launch-reason").is_empty());
        window.click("agents-launch-submit", cx);
        window.render_frame(cx);
        let message = window
            .find("agents-launch-outcome")
            .label()
            .map(str::to_owned);
        assert_eq!(message.as_deref(), Some("Enter a name. Enter a model."));
        assert!(client.requests.lock().unwrap().is_empty());

        let (name, model, egress) = view.read_with(cx, |v, _| {
            (
                v.name_input().clone(),
                v.model_input().clone(),
                v.egress_input().clone(),
            )
        });
        name.update(cx, |i, cx| i.set_value("reviewer", window, cx));
        model.update(cx, |i, cx| i.set_value("claude-opus", window, cx));
        egress.update(cx, |i, cx| i.set_value("api.github.com", window, cx));
        window.click("agents-launch-submit", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("agents-launch-outcome").label(),
            Some("Launched agent ag_42.")
        );
    })
    .expect("window");
    let requests = client.requests.lock().unwrap();
    assert_eq!(requests.len(), 1);
    assert_eq!(requests[0].name, "reviewer");
    assert_eq!(requests[0].harness, "claude");
    assert_eq!(requests[0].egress_allow, ["api.github.com"]);
}

#[gpui_kit::test]
fn a_failing_client_is_reported(cx: &mut TestAppContext) {
    let client = FakeManagedClient::failing("no such route");
    let (window, view) = open(cx, KEY, client.clone());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let (name, model) =
            view.read_with(cx, |v, _| (v.name_input().clone(), v.model_input().clone()));
        name.update(cx, |i, cx| i.set_value("a", window, cx));
        model.update(cx, |i, cx| i.set_value("m", window, cx));
        window.click("agents-launch-submit", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("agents-launch-outcome").label(),
            Some("Could not launch the agent: no such route")
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn setting_the_key_flag_enables_the_form(cx: &mut TestAppContext) {
    let (window, view) = open(cx, LaunchConfig::default(), FakeManagedClient::ok("x"));
    view.update(cx, |v, cx| v.set_model_key_configured(true, cx));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.find_all("agents-launch-reason").is_empty());
    })
    .expect("window");
    assert!(view.read_with(cx, |v, _| v.launch_enabled()));
}
