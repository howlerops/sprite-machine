//! M3-005: the thread composer's slash commands, mode / model pickers, queued chips and Stop.
//! Pure decisions are tested natively; the view headlessly, with the events it emits recorded.

use std::sync::{Arc, Mutex};

use client_core::ThreadView;
use client_core::outbox::Intent;
use client_gpui::compose::{
    ComposerEvent, SessionOptions, SlashAccept, SlashCommand, ThreadComposer, ThreadRequest,
    accept_command, cap_from_features, filter_commands, queued_chips, remove_request, running_task,
    slash_query, stop_intent,
};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};
use protocol::turn_event::Envelope;
use serde_json::{Map, Value, json};

// ---- fixtures ----------------------------------------------------------------------------------

fn session_state() -> Value {
    json!({
        "available_commands": [
            {"name": "compact", "description": "Summarize the conversation"},
            {"name": "review", "description": "Review changes", "input": {"hint": "what to review"}},
            {"name": "init", "description": "Create a compact AGENTS file"}
        ],
        "modes": [
            {"id": "plan", "name": "Plan"},
            {"id": "default", "name": "Default"},
            {"id": "acceptEdits", "name": "Accept edits"},
            {"id": "bypassPermissions", "name": "Bypass"}
        ],
        "current_mode_id": "default",
        "config_options": [
            {"id": "effort", "name": "Effort", "currentValue": "low",
             "options": [{"value": "low", "name": "Low"}]},
            {"id": "model", "name": "Model", "currentValue": "sonnet",
             "options": [
                {"value": "sonnet", "name": "Sonnet"},
                {"group": "g", "name": "More", "options": [{"value": "opus", "name": "Opus"}]}
             ]}
        ]
    })
}

fn example(file: &str) -> Envelope {
    let path: std::path::PathBuf = [
        env!("CARGO_MANIFEST_DIR"),
        "..",
        "protocol",
        "schemas",
        "turn-event",
        "examples",
        file,
    ]
    .iter()
    .collect();
    serde_json::from_str(&std::fs::read_to_string(path).expect("example")).expect("envelope")
}

fn thread(running: bool, pending: Value) -> ThreadView {
    let mut view = ThreadView::new("th_1");
    let mut states = Map::new();
    states.insert("session".into(), session_state());
    states.insert("thread".into(), json!({ "pending_tasks": pending }));
    view.install_snapshot(states);
    if running {
        view.apply(&example("RunStarted.json"))
            .expect("run started");
    }
    view
}

fn two_pending() -> Value {
    json!([
        {"task_id": "tk_2", "preview": "then add tests"},
        {"task_id": "tk_3", "preview": "and a changelog"}
    ])
}

// ---- native ------------------------------------------------------------------------------------

#[test]
fn the_session_state_gives_commands_modes_and_the_model() {
    let options = SessionOptions::from_state(Some(&session_state()));
    assert_eq!(options.commands.len(), 3);
    assert_eq!(
        options.commands[1].input_hint.as_deref(),
        Some("what to review")
    );
    assert_eq!(options.modes.len(), 4);
    assert_eq!(options.current_mode.as_deref(), Some("default"));
    let model = options.model.expect("model");
    assert_eq!(model.id, "model");
    assert_eq!(model.current.as_deref(), Some("sonnet"));
    let values: Vec<_> = model.options.iter().map(|o| o.value.as_str()).collect();
    assert_eq!(
        values,
        ["sonnet", "opus"],
        "grouped values are flattened; effort is not the model"
    );
    assert_eq!(SessionOptions::from_state(None), SessionOptions::default());
}

#[test]
fn slash_lists_start_with_prefix_matches_and_stop_at_a_space() {
    assert_eq!(slash_query("/"), Some(""));
    assert_eq!(slash_query("/re"), Some("re"));
    assert_eq!(slash_query("/review now"), None);
    assert_eq!(slash_query("hello /x"), None);
    let commands = SessionOptions::from_state(Some(&session_state())).commands;
    let names = |q: &str| -> Vec<String> {
        filter_commands(&commands, q)
            .iter()
            .map(|c| c.name.clone())
            .collect()
    };
    assert_eq!(names(""), ["compact", "review", "init"]);
    assert_eq!(
        names("co"),
        ["compact", "init"],
        "name prefix, then description match"
    );
    assert_eq!(names("REV"), ["review"]);
    assert!(names("zzz").is_empty());
}

#[test]
fn a_command_with_a_hint_waits_for_arguments_and_one_without_is_sent() {
    let commands = SessionOptions::from_state(Some(&session_state())).commands;
    let find = |n: &str| -> &SlashCommand { commands.iter().find(|c| c.name == n).expect("cmd") };
    assert_eq!(
        accept_command(find("review")),
        SlashAccept::Insert("/review ".into())
    );
    assert_eq!(
        accept_command(find("compact")),
        SlashAccept::Send("/compact".into())
    );
}

#[test]
fn modes_looser_than_the_runner_cap_are_not_offered() {
    let options = SessionOptions::from_state(Some(&session_state()));
    let ids = |cap: Option<&str>| -> Vec<String> {
        options
            .allowed_modes(cap)
            .iter()
            .map(|m| m.id.clone())
            .collect()
    };
    assert_eq!(ids(None).len(), 4);
    let capped = ids(Some("default"));
    assert!(capped.contains(&"plan".to_owned()) && capped.contains(&"default".to_owned()));
    assert!(!capped.contains(&"bypassPermissions".to_owned()));
    assert_eq!(
        cap_from_features(&["x", "permission_mode:acceptEdits", "permission_mode:plan"]).as_deref(),
        Some("plan")
    );
    assert_eq!(cap_from_features::<&str>(&[]), None);
}

#[test]
fn queued_chips_are_removable_and_stop_targets_the_running_task() {
    let view = thread(true, two_pending());
    let chips = queued_chips(&view);
    assert_eq!(
        chips.iter().map(|c| c.task_id.as_str()).collect::<Vec<_>>(),
        ["tk_2", "tk_3"]
    );
    assert_eq!(chips[0].preview, "then add tests");
    assert_eq!(
        remove_request(&view, "tk_3"),
        Some(ThreadRequest::RemovePending {
            thread_id: "th_1".into(),
            task_id: "tk_3".into()
        })
    );
    assert_eq!(
        remove_request(&view, "tk_9"),
        None,
        "only queued messages can be removed"
    );
    assert_eq!(running_task(&view), Some("tk_1"));
    assert_eq!(
        stop_intent(&view),
        Some(Intent::Cancel {
            thread_id: "th_1".into(),
            task_id: "tk_1".into()
        })
    );
    let idle = thread(false, json!([]));
    assert_eq!(stop_intent(&idle), None);
    assert!(queued_chips(&idle).is_empty());
}

#[test]
fn requests_have_the_hub_shapes() {
    let mode = ThreadRequest::SetMode {
        thread_id: "th_1".into(),
        mode_id: "plan".into(),
    };
    assert_eq!(
        (mode.method(), mode.path()),
        ("POST", "/threads/th_1/session".to_owned())
    );
    assert_eq!(mode.body(), Some(json!({"mode_id": "plan"})));
    let model = ThreadRequest::SetConfig {
        thread_id: "th_1".into(),
        id: "model".into(),
        value: "opus".into(),
    };
    assert_eq!(
        model.body(),
        Some(json!({"config": {"id": "model", "value": "opus"}}))
    );
    let remove = ThreadRequest::RemovePending {
        thread_id: "th_1".into(),
        task_id: "tk_2".into(),
    };
    assert_eq!(
        (remove.method(), remove.path()),
        ("DELETE", "/threads/th_1/pending/tk_2".to_owned())
    );
    assert_eq!(remove.body(), None);
}

#[test]
fn every_thread_composer_string_id_is_in_the_catalog() {
    let source = include_str!("../locales/en/compose.ftl");
    let catalog = Catalog::from_source("en", source).expect("parses");
    let code = std::fs::read_to_string(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/src/compose/thread.rs"
    ))
    .expect("thread.rs");
    let mut seen = 0;
    for part in code.split("\"compose-thread-").skip(1) {
        let id = format!(
            "compose-thread-{}",
            part.split('"').next().unwrap_or_default()
        );
        assert!(
            source.contains(&format!("{id} =")),
            "{id} missing from compose.ftl"
        );
        let args = [
            ("name", "n"),
            ("label", "l"),
            ("description", "d"),
            ("preview", "p"),
            ("shortcut", "s"),
        ];
        assert!(!catalog.tr(&id, &args).is_empty());
        seen += 1;
    }
    assert!(seen > 8);
}

// ---- headless ----------------------------------------------------------------------------------

type Events = Arc<Mutex<Vec<ComposerEvent>>>;

fn open(
    cx: &mut TestAppContext,
    view: &ThreadView,
    cap: Option<&str>,
) -> (AnyWindowHandle, Entity<ThreadComposer>, Events) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::compose::init(cx);
    });
    let (window, composer) = cx.update(|cx| {
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(Bounds {
                    origin: Point::default(),
                    size: size(px(900.), px(900.)),
                })),
                ..Default::default()
            },
            cx,
            |window, cx| cx.new(|cx| ThreadComposer::new(window, cx)),
        )
        .expect("window")
    });
    let events: Events = Arc::default();
    let sink = events.clone();
    cx.update(|cx| {
        cx.subscribe(&composer, move |_, event: &ComposerEvent, _| {
            sink.lock().expect("lock").push(event.clone());
        })
        .detach();
    });
    let cap = cap.map(str::to_owned);
    cx.update_window(window, |_, window, cx| {
        composer.update(cx, |composer, cx| {
            composer.set_runner_mode(cap, cx);
            composer.set_thread(view, cx);
        });
        window.render_frame(cx);
    })
    .expect("window");
    (window, composer, events)
}

fn type_text(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    composer: &Entity<ThreadComposer>,
    text: &str,
) {
    cx.update_window(window, |_, window, cx| {
        let focus = composer.read(cx).prompt_input().focus_handle(cx);
        window.focus(&focus, cx);
        window.input(text, cx);
        window.render_frame(cx);
    })
    .expect("window");
}

#[gpui_kit::test]
fn slash_opens_the_list_and_clicking_a_command_sends_or_inserts(cx: &mut TestAppContext) {
    let view = thread(false, json!([]));
    let (window, composer, events) = open(cx, &view, None);
    type_text(cx, window, &composer, "/");
    cx.update_window(window, |_, window, cx| {
        assert!(window.find("thread-slash").visible());
        assert_eq!(composer.read(cx).slash_matches(cx).len(), 3);
        assert_eq!(
            window.find("slash-review").label(),
            Some("Command /review: Review changes")
        );
        window.click("slash-compact", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [ComposerEvent::Send {
            thread_id: "th_1".into(),
            text: "/compact".into()
        }]
    );
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            composer.read(cx).prompt_input().read(cx).value().as_ref(),
            ""
        );
        window.input("/rev", cx);
        window.render_frame(cx);
        window.click("slash-review", cx);
        window.render_frame(cx);
        assert_eq!(
            composer.read(cx).prompt_input().read(cx).value().as_ref(),
            "/review "
        );
        assert!(
            composer.read(cx).slash_matches(cx).is_empty(),
            "the list closes at the space"
        );
    })
    .expect("window");
    assert_eq!(
        events.lock().expect("lock").len(),
        1,
        "a command with a hint is not sent yet"
    );
}

#[gpui_kit::test]
fn enter_takes_the_highlighted_command_and_arrows_move_it(cx: &mut TestAppContext) {
    let view = thread(false, json!([]));
    let (window, composer, events) = open(cx, &view, None);
    type_text(cx, window, &composer, "/");
    cx.update_window(window, |_, window, cx| {
        window.press("down", cx);
        window.render_frame(cx);
        assert_eq!(composer.read(cx).slash_selected(), 1);
        window.press("up", cx);
        window.press("up", cx);
        assert_eq!(composer.read(cx).slash_selected(), 2, "wraps");
        window.press("enter", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [ComposerEvent::Send {
            thread_id: "th_1".into(),
            text: "/init".into()
        }]
    );
}

#[gpui_kit::test]
fn the_pickers_ask_for_changes_and_hide_modes_beyond_the_runner(cx: &mut TestAppContext) {
    let view = thread(false, json!([]));
    let (window, _composer, events) = open(cx, &view, Some("acceptEdits"));
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("thread-mode-default").label(),
            Some("Mode Default, selected")
        );
        assert!(window.try_find("thread-mode-bypassPermissions").is_none());
        assert_eq!(
            window.find("thread-model-sonnet").label(),
            Some("Model Sonnet, selected")
        );
        window.click("thread-mode-plan", cx);
        window.click("thread-model-opus", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [
            ComposerEvent::Request(ThreadRequest::SetMode {
                thread_id: "th_1".into(),
                mode_id: "plan".into()
            }),
            ComposerEvent::Request(ThreadRequest::SetConfig {
                thread_id: "th_1".into(),
                id: "model".into(),
                value: "opus".into()
            }),
        ]
    );
}

#[gpui_kit::test]
fn queued_chips_remove_and_stop_cancels(cx: &mut TestAppContext) {
    let view = thread(true, two_pending());
    let (window, _composer, events) = open(cx, &view, None);
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("queued-tk_2").label(),
            Some("Queued message: then add tests")
        );
        window.click("queued-remove-tk_3", cx);
        window.click("thread-stop", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [
            ComposerEvent::Request(ThreadRequest::RemovePending {
                thread_id: "th_1".into(),
                task_id: "tk_3".into()
            }),
            ComposerEvent::Cancel(Intent::Cancel {
                thread_id: "th_1".into(),
                task_id: "tk_1".into()
            }),
        ]
    );
}

#[gpui_kit::test]
fn no_stop_button_and_no_chips_when_idle(cx: &mut TestAppContext) {
    let view = thread(false, json!([]));
    let (window, _composer, _events) = open(cx, &view, None);
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("thread-stop").is_none());
        assert!(window.try_find("thread-queue").is_none());
    })
    .expect("window");
}
