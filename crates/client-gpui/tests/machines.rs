//! M2B-013: dispatch on the desktop. The Where picker (machines with presence, a disabled cloud
//! choice, the chosen machine and spawn mode in the create request), the Machines screen (list,
//! rename, revoke behind a confirmation), the inbox's computer icon, and the task view's
//! "waiting for <machine>" banner. Everything runs against a local fake hub on GPUI's headless
//! platform.

use std::collections::BTreeSet;
use std::sync::atomic::{AtomicU16, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use client_core::outbox::ManualClock;
use client_gpui::compose::{ComposeView, Status, Target};
use client_gpui::inbox::InboxView;
use client_gpui::machines::{
    self, Action, MAX_NAME_CHARS, MachinesEvent, MachinesView, NameError, PresenceState, Screen,
    rows_from, validate_name,
};
use client_gpui::shell::auth::SessionGrant;
use client_gpui::shell::browser::RecordingBrowser;
use client_gpui::shell::i18n::Catalog;
use client_gpui::shell::prefs::Preferences;
use client_gpui::shell::session::{MemoryStore, Session, SessionStore};
use client_gpui::shell::{AppConfig, AppShell};
use client_gpui::task::{WAITING_ITEM_ID, waiting_for};
use gpui_kit::Focusable as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Point, TestAppContext, WindowBounds,
    WindowOptions, px, size,
};
use serde_json::{Value, json};

// ---- a fake hub -------------------------------------------------------------------------------

#[derive(Default)]
struct Hub {
    /// The `machines` array `GET /v1/machines` answers with; PATCH renames, DELETE removes.
    machines: Mutex<Vec<Value>>,
    /// `PATCH` / `DELETE` /v1/machines/{id}: method, id, headers, body.
    writes: Mutex<Vec<(String, String, HeaderMap, Value)>>,
    /// The status PATCH and DELETE answer (0 means 200).
    write_status: AtomicU16,
    /// `POST /v1/threads` bodies.
    creates: Mutex<Vec<(HeaderMap, Value)>>,
    /// The rows `GET /v1/threads` answers with.
    threads: Mutex<Vec<Value>>,
    /// Event frames the thread subscription replays, by thread.
    frames: Mutex<std::collections::HashMap<String, Vec<String>>>,
}

async fn list_machines(State(hub): State<Arc<Hub>>) -> Json<Value> {
    Json(json!({ "machines": hub.machines.lock().expect("lock").clone() }))
}

async fn write_machine(
    State(hub): State<Arc<Hub>>,
    method: axum::http::Method,
    Path(id): Path<String>,
    headers: HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    let body: Value = serde_json::from_slice(&body).unwrap_or(Value::Null);
    hub.writes
        .lock()
        .expect("lock")
        .push((method.to_string(), id.clone(), headers, body.clone()));
    let status = hub.write_status.load(Ordering::SeqCst);
    if status != 0 {
        return (
            StatusCode::from_u16(status).expect("status"),
            Json(json!({ "error": "forbidden", "message": "admin role required" })),
        )
            .into_response();
    }
    let mut machines = hub.machines.lock().expect("lock");
    if method == axum::http::Method::DELETE {
        machines.retain(|m| m["machine_id"] != id.as_str());
    } else if let Some(machine) = machines.iter_mut().find(|m| m["machine_id"] == id.as_str()) {
        machine["name"] = body["name"].clone();
    }
    Json(json!({})).into_response()
}

async fn create_thread(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    hub.creates.lock().expect("lock").push((headers, body));
    (
        StatusCode::CREATED,
        Json(json!({ "thread_id": "th_new", "task_id": "tk_x", "status": "started" })),
    )
}

async fn list_threads(State(hub): State<Arc<Hub>>) -> Json<Value> {
    Json(json!({ "threads": hub.threads.lock().expect("lock").clone(), "next_cursor": null }))
}

async fn subscribe(
    State(hub): State<Arc<Hub>>,
    Path(thread_id): Path<String>,
    upgrade: WebSocketUpgrade,
) -> Response {
    let frames = hub
        .frames
        .lock()
        .expect("lock")
        .get(&thread_id)
        .cloned()
        .unwrap_or_default();
    upgrade.on_upgrade(move |mut socket| async move {
        if !frames.is_empty() {
            let hello = json!({ "type": "hello", "head_seq": frames.len(), "replay_from": 1 });
            let _ = socket.send(WsMessage::Text(hello.to_string().into())).await;
            for frame in frames {
                let _ = socket.send(WsMessage::Text(frame.into())).await;
            }
        }
        let _ = socket
            .send(WsMessage::Text(r#"{"type":"live"}"#.into()))
            .await;
        // Stay open (a timeout is fine) until the client closes or errors.
        while !matches!(
            tokio::time::timeout(Duration::from_millis(50), socket.recv()).await,
            Ok(None | Some(Err(_)))
        ) {}
    })
}

/// Starts the hub; returns its root URL (`http://127.0.0.1:port`).
fn spawn_hub(hub: Arc<Hub>) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("hub runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/v1/machines", get(list_machines))
                .route(
                    "/v1/machines/{id}",
                    axum::routing::patch(write_machine).delete(write_machine),
                )
                .route("/v1/threads", post(create_thread).get(list_threads))
                .route("/v1/threads/{thread_id}/subscribe", get(subscribe))
                .with_state(hub);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind hub");
            sender
                .send(listener.local_addr().expect("addr").port())
                .expect("send port");
            axum::serve(listener, app).await.expect("serve");
        });
    });
    format!("http://127.0.0.1:{}", receiver.recv().expect("hub port"))
}

fn laptop(presence: &str) -> Value {
    json!({
        "machine_id": "mch_laptop", "name": "Laptop", "kind": "local",
        "presence": { "state": presence, "since": "2026-10-27T22:00:00.000Z" },
        "capabilities": {
            "harnesses": ["claude", "opencode"],
            "spawn": ["same_dir", "worktree"],
            "max_sessions": 32,
        },
    })
}

fn old_box() -> Value {
    json!({
        "machine_id": "mch_old", "name": "old-box", "kind": "local",
        "presence": { "state": "offline", "since": "2026-10-20T08:00:00.000Z" },
        "capabilities": { "harnesses": ["claude"], "spawn": ["same_dir"], "max_sessions": 4 },
    })
}

fn hub_with(machines: Vec<Value>) -> (Arc<Hub>, String) {
    let hub = Arc::new(Hub::default());
    *hub.machines.lock().expect("lock") = machines;
    let root = spawn_hub(hub.clone());
    (hub, root)
}

// ---- models, natively -------------------------------------------------------------------------

#[test]
fn names_are_trimmed_and_bounded_like_the_hubs_rule() {
    assert_eq!(validate_name("  desk  "), Ok("desk".to_owned()));
    assert_eq!(validate_name("   "), Err(NameError::Empty));
    assert_eq!(validate_name(""), Err(NameError::Empty));
    assert_eq!(
        validate_name(&"x".repeat(MAX_NAME_CHARS)).map(|n| n.len()),
        Ok(128)
    );
    assert_eq!(
        validate_name(&"x".repeat(MAX_NAME_CHARS + 1)),
        Err(NameError::TooLong)
    );
}

#[test]
fn rows_carry_presence_kind_and_harnesses() {
    let body = json!({ "machines": [laptop("online"), laptop("asleep"), old_box(),
        { "machine_id": "mch_x", "name": "x", "presence": { "state": "weird" }, "capabilities": null }] });
    let response = client_gpui::agents::MachinesResponse::parse(&body.to_string()).expect("parse");
    let rows = rows_from(&response.machines);
    let states: Vec<_> = rows.iter().map(|row| row.presence).collect();
    assert_eq!(
        states,
        [
            PresenceState::Online,
            PresenceState::Asleep,
            PresenceState::Offline,
            PresenceState::Unknown
        ]
    );
    assert_eq!(rows[0].harnesses, ["claude", "opencode"]);
    assert!(rows[3].harnesses.is_empty());
    assert_eq!(rows[2].since.as_deref(), Some("2026-10-20T08:00:00.000Z"));
}

#[test]
fn waiting_only_for_a_machine_that_is_not_online() {
    use client_gpui::compose::parse_machines;
    let machines = parse_machines(&json!({ "machines": [laptop("online"), old_box()] }));
    let items = vec!["then add tests".to_owned()];
    assert_eq!(waiting_for(Some(&machines[0]), items.clone()), None);
    let waiting = waiting_for(Some(&machines[1]), items.clone()).expect("offline");
    assert_eq!(waiting.machine_name, "old-box");
    assert_eq!(waiting.items, items);
    assert_eq!(waiting_for(None, items), None, "a machine not in the feed");
}

#[test]
fn every_string_id_in_the_machines_code_is_in_the_catalog_and_every_catalog_id_is_used() {
    let catalog = Catalog::from_source("en", machines::EN_FTL).expect("machines.ftl parses");
    let sources = [
        include_str!("../src/machines/view.rs"),
        include_str!("../src/machines/model.rs"),
        include_str!("../src/task/waiting.rs"),
    ];
    let mut used = BTreeSet::new();
    for source in sources {
        let mut rest = source;
        while let Some(start) = rest.find('"') {
            let tail = &rest[start + 1..];
            let end = tail.find('"').expect("closing quote");
            let literal = &tail[..end];
            if literal.starts_with("machines-")
                && literal
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                used.insert(literal.to_owned());
            }
            rest = &tail[end + 1..];
        }
    }
    // Ids built from a prefix: the presence and the `since` variants.
    for state in ["online", "asleep", "offline", "unknown"] {
        used.insert(format!("machines-presence-{state}"));
    }
    // Element ids share the prefix but are not strings.
    for element in [
        "machines-status",
        "machines-revoke-prompt",
        "machines-rename-input",
        "machines-empty",
        "machines-header",
        "machines-title",
        "machines-table",
        "machines-rename-save",
        "machines-rename-cancel",
        "machines-revoke-yes",
        "machines-revoke-cancel",
    ] {
        if !catalog.has(element) {
            used.remove(element);
        }
    }
    for id in &used {
        assert!(catalog.has(id), "string id {id} is not in machines.ftl");
    }
    for id in Catalog::ids(machines::EN_FTL) {
        assert!(
            used.contains(&id),
            "machines.ftl id {id} is not used by the code"
        );
    }
}

#[test]
fn the_compose_catalog_has_the_where_strings() {
    let catalog = Catalog::from_source("en", client_gpui::compose::EN_FTL).expect("parses");
    for id in [
        "compose-where-cloud",
        "compose-where-cloud-a11y",
        "compose-where-cloud-a11y-unavailable",
        "compose-where-cloud-unavailable",
        "compose-where-waits",
        "compose-machine-offline-a11y",
        "compose-machine-offline-selected-a11y",
    ] {
        assert!(catalog.has(id), "{id}");
    }
    assert!(
        !catalog.has("compose-invalid-machine-offline"),
        "an offline machine no longer blocks a send"
    );
}

// ---- the Where picker -------------------------------------------------------------------------

fn open_composer(cx: &mut TestAppContext) -> (AnyWindowHandle, Entity<ComposeView>) {
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::compose::init(cx);
    });
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
            move |window, cx| cx.new(|cx| ComposeView::new(None, window, cx)),
        )
        .expect("open window")
    })
}

fn wait_until_composer(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<ComposeView>,
    done: impl Fn(&mut gpui_kit::Window, &mut gpui_kit::App) -> bool,
) {
    for _ in 0..400 {
        let finished = cx
            .update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.pump(window, cx));
                window.render_frame(cx);
                done(window, cx)
            })
            .expect("window");
        if finished {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out");
}

#[cfg(target_os = "macos")]
fn shortcut() -> &'static str {
    "cmd-enter"
}

#[cfg(not(target_os = "macos"))]
fn shortcut() -> &'static str {
    "ctrl-enter"
}

fn type_task(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<ComposeView>,
    prompt: &str,
) {
    cx.update_window(window, |_, window, cx| {
        let workspace = view.read(cx).workspace_input().focus_handle(cx);
        window.focus(&workspace, cx);
        window.input("/Users/me/src/app", cx);
        let field = view.read(cx).prompt_input().focus_handle(cx);
        window.focus(&field, cx);
        window.input(prompt, cx);
        window.render_frame(cx);
    })
    .expect("window");
    // Change events reach the model between updates; the shortcut sends what they carried.
    cx.update_window(window, |_, window, cx| window.press(shortcut(), cx))
        .expect("window");
}

#[gpui_kit::test]
fn the_where_picker_lists_machines_with_presence_and_a_disabled_cloud_choice(
    cx: &mut TestAppContext,
) {
    let (hub, root) = hub_with(vec![old_box(), laptop("online")]);
    let (window, view) = open_composer(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api: format!("{root}/v1"),
                    token: Some("tok".to_owned()),
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    wait_until_composer(cx, window, &view, |window, cx| {
        view.read(cx).model().selected_machine().is_some()
            && window.find("machine-mch_laptop").visible()
    });

    cx.update_window(window, |_, window, cx| {
        // Presence is in the label the user sees and in the one a screen reader gets.
        assert_eq!(
            window.find("machine-mch_old").label(),
            Some("Run on old-box, offline")
        );
        assert_eq!(
            window.find("machine-mch_laptop").label(),
            Some("Run on Laptop, selected")
        );
        // Cloud workspaces: listed, disabled, with the reason.
        let cloud = window.find("where-cloud").label().map(str::to_owned);
        assert_eq!(
            cloud.as_deref(),
            Some("Cloud workspace, unavailable: Cloud workspaces are not available yet.")
        );
        assert_eq!(
            window.find("where-cloud-reason").label(),
            Some("Cloud workspaces are not available yet.")
        );
        window.click("where-cloud", cx);
        window.render_frame(cx);
        assert_eq!(
            view.read(cx).model().draft().machine_id.as_deref(),
            Some("mch_laptop"),
            "the disabled choice changes nothing"
        );
        assert!(
            window.try_find("where-wait-note").is_none(),
            "online: no note"
        );
    })
    .expect("window");

    // A worktree task on the online machine: the choices go into the create request.
    cx.update_window(window, |_, window, cx| {
        window.click("spawn-worktree", cx);
    })
    .expect("window");
    type_task(cx, window, &view, "Upgrade axum");
    wait_until_composer(cx, window, &view, |_, cx| {
        matches!(view.read(cx).status(), Status::Sent { .. })
    });
    {
        let creates = hub.creates.lock().expect("lock");
        assert_eq!(creates.len(), 1);
        assert_eq!(creates[0].1["machine_id"], "mch_laptop");
        assert_eq!(creates[0].1["workspace"]["spawn"], "worktree");
        assert_eq!(creates[0].0["authorization"], "Bearer tok");
    }

    // An offline machine can be chosen: the picker says the task will wait, and the send goes.
    cx.update_window(window, |_, window, cx| {
        window.click("machine-mch_old", cx);
        window.render_frame(cx);
        let note = window.find("where-wait-note").label().map(str::to_owned);
        assert_eq!(
            note.as_deref(),
            Some("old-box is offline. The task will wait on the hub and start when it is back.")
        );
        assert_eq!(
            window.find("machine-mch_old").label(),
            Some("Run on old-box, offline, selected")
        );
        window.click("spawn-same_dir", cx);
    })
    .expect("window");
    type_task(cx, window, &view, "Fix the flaky test");
    wait_until_composer(cx, window, &view, |_, _| {
        hub.creates.lock().expect("lock").len() == 2
    });
    let creates = hub.creates.lock().expect("lock");
    assert_eq!(creates[1].1["machine_id"], "mch_old");
    assert_eq!(creates[1].1["workspace"]["spawn"], "same_dir");
}

// ---- the Machines screen ----------------------------------------------------------------------

fn open_machines(
    cx: &mut TestAppContext,
    root: &str,
) -> (
    AnyWindowHandle,
    Entity<MachinesView>,
    Arc<Mutex<Vec<MachinesEvent>>>,
    gpui_kit::Subscription,
) {
    cx.executor().allow_parking();
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1100.), px(700.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| MachinesView::new(window, cx)),
        )
        .expect("open window")
    });
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let subscription = cx.update(|cx| {
        cx.subscribe(&view, move |_, event: &MachinesEvent, _| {
            sink.lock().expect("lock").push(event.clone());
        })
    });
    let info = machines_info(&[laptop("online"), old_box()]);
    let target = Target {
        api: format!("{root}/v1"),
        token: Some("tok".to_owned()),
    };
    cx.update(|cx| {
        view.update(cx, |view, cx| {
            view.set_target(Some(target), cx);
            view.set_machines(&info, cx);
        });
    });
    (window, view, events, subscription)
}

fn machines_info(machines: &[Value]) -> Vec<client_gpui::agents::MachineInfo> {
    client_gpui::agents::MachinesResponse::parse(&json!({ "machines": machines }).to_string())
        .expect("parse")
        .machines
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> Option<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find(id)
            .and_then(|found| found.label().map(str::to_owned))
    })
    .expect("window")
}

/// Advances the test clock (the view polls its request on a 50 ms timer) until `done`.
fn pump_until(cx: &mut TestAppContext, what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

#[gpui_kit::test]
fn the_machines_screen_lists_machines_with_presence(cx: &mut TestAppContext) {
    let (_hub, root) = hub_with(vec![]);
    let (window, _view, _events, _sub) = open_machines(cx, &root);
    assert_eq!(
        label(cx, window, "machine-row-mch_laptop").as_deref(),
        Some("Laptop, Computer, Online since 2026-10-27T22:00:00.000Z")
    );
    assert_eq!(
        label(cx, window, "machine-row-mch_old").as_deref(),
        Some("old-box, Computer, Offline since 2026-10-20T08:00:00.000Z")
    );
    assert_eq!(
        label(cx, window, "machine-rename-mch_old").as_deref(),
        Some("Rename old-box")
    );
    assert_eq!(
        label(cx, window, "machine-revoke-mch_old").as_deref(),
        Some("Revoke old-box")
    );
}

#[gpui_kit::test]
fn renaming_sends_a_patch_with_the_trimmed_name_and_tells_the_owner(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![laptop("online"), old_box()]);
    let (window, view, events, _sub) = open_machines(cx, &root);

    cx.update_window(window, |_, window, cx| {
        window.click("machine-rename-mch_old", cx)
    })
    .expect("window");
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            view.read(cx).screen(),
            &Screen::Renaming {
                machine_id: "mch_old".to_owned()
            }
        );
        window.render_frame(cx);
        assert_eq!(
            window.find("machines-rename-input").label(),
            Some("New name for old-box")
        );
        assert_eq!(
            view.read(cx).name_input().read(cx).value().to_string(),
            "old-box",
            "the field starts with the current name"
        );
        // An empty name is refused before anything is sent.
        view.read(cx).name_input().clone().update(cx, |input, cx| {
            input.set_value("   ", window, cx);
        });
        window.click("machines-rename-save", cx);
    })
    .expect("window");
    assert_eq!(
        label(cx, window, "machines-status").as_deref(),
        Some("Enter a name.")
    );
    assert!(hub.writes.lock().expect("lock").is_empty());

    cx.update_window(window, |_, window, cx| {
        view.read(cx).name_input().clone().update(cx, |input, cx| {
            input.set_value("  desk at home ", window, cx);
        });
        window.click("machines-rename-save", cx);
    })
    .expect("window");
    pump_until(cx, "the rename", || {
        !events.lock().expect("lock").is_empty()
    });

    let writes = hub.writes.lock().expect("lock");
    assert_eq!(writes.len(), 1);
    let (method, id, headers, body) = &writes[0];
    assert_eq!((method.as_str(), id.as_str()), ("PATCH", "mch_old"));
    assert_eq!(body, &json!({ "name": "desk at home" }));
    assert_eq!(headers["authorization"], "Bearer tok");
    drop(writes);
    assert_eq!(
        events.lock().expect("lock").as_slice(),
        [MachinesEvent::Changed]
    );
    assert_eq!(
        label(cx, window, "machines-status").as_deref(),
        Some("Renamed to desk at home.")
    );
    cx.update(|cx| assert_eq!(view.read(cx).screen(), &Screen::List));
}

#[gpui_kit::test]
fn revoking_asks_first_and_sends_a_delete_only_after_the_confirmation(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![laptop("online"), old_box()]);
    let (window, view, events, _sub) = open_machines(cx, &root);

    cx.update_window(window, |_, window, cx| {
        window.click("machine-revoke-mch_old", cx)
    })
    .expect("window");
    let prompt = label(cx, window, "machines-revoke-prompt");
    assert!(
        prompt
            .as_deref()
            .is_some_and(|p| p.starts_with("Revoke old-box?")),
        "{prompt:?}"
    );
    assert_eq!(
        label(cx, window, "machines-revoke-yes").as_deref(),
        Some("Confirm: revoke old-box")
    );
    assert!(
        hub.writes.lock().expect("lock").is_empty(),
        "asking sends nothing"
    );

    // Cancel: back to the list, still nothing sent.
    cx.update_window(window, |_, window, cx| {
        window.click("machines-revoke-cancel", cx)
    })
    .expect("window");
    assert_eq!(label(cx, window, "machines-revoke-prompt"), None);
    assert!(hub.writes.lock().expect("lock").is_empty());

    // Ask again and confirm.
    cx.update_window(window, |_, window, cx| {
        window.click("machine-revoke-mch_old", cx);
        window.render_frame(cx);
        window.click("machines-revoke-yes", cx);
    })
    .expect("window");
    pump_until(cx, "the revoke", || {
        !events.lock().expect("lock").is_empty()
    });
    let writes = hub.writes.lock().expect("lock");
    assert_eq!(writes.len(), 1);
    let (method, id, headers, _) = &writes[0];
    assert_eq!((method.as_str(), id.as_str()), ("DELETE", "mch_old"));
    assert!(
        headers.contains_key("idempotency-key"),
        "a DELETE carries an Idempotency-Key (client-api.md 2)"
    );
    drop(writes);
    assert_eq!(
        label(cx, window, "machines-status").as_deref(),
        Some("Revoked old-box.")
    );
    cx.update(|cx| assert_eq!(view.read(cx).screen(), &Screen::List));
}

#[gpui_kit::test]
fn a_refused_revoke_says_why_and_keeps_the_confirmation_open(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![laptop("online"), old_box()]);
    hub.write_status.store(403, Ordering::SeqCst);
    let (window, view, events, _sub) = open_machines(cx, &root);
    cx.update_window(window, |_, window, cx| {
        window.click("machine-revoke-mch_old", cx);
        window.render_frame(cx);
        window.click("machines-revoke-yes", cx);
    })
    .expect("window");
    pump_for_label(cx, window, "machines-status", |l| {
        l == Some("Only admins can rename or revoke machines.")
    });
    assert!(events.lock().expect("lock").is_empty(), "nothing changed");
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).screen(),
            &Screen::ConfirmRevoke {
                machine_id: "mch_old".to_owned()
            }
        );
    });
    let _ = Action::Revoke {
        machine_id: String::new(),
        name: String::new(),
    };
}

// ---- the inbox's computer icon ----------------------------------------------------------------

#[gpui_kit::test]
fn machine_hosted_inbox_rows_show_a_labelled_computer_icon(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![]);
    *hub.threads.lock().expect("lock") = vec![
        json!({ "thread_id": "th_1", "title": "On the laptop", "machine_id": "mch_laptop",
                "agent": "claude", "status": "running",
                "updated_at": "2026-10-28T10:00:00.000Z", "last_seq": 7 }),
        json!({ "thread_id": "th_2", "title": "No host known", "machine_id": "",
                "agent": "claude", "status": "done",
                "updated_at": "2026-10-28T09:00:00.000Z", "last_seq": 3 }),
    ];
    cx.update(gpui_kit::init);
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1100.), px(900.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| cx.new(|cx| InboxView::new(window, cx)),
        )
        .expect("open window")
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(client_gpui::inbox::Target {
                    api: format!("{root}/v1"),
                    token: Some("tok".to_owned()),
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    let mut icons = Vec::new();
    let mut rows = 0;
    for _ in 0..400 {
        (rows, icons) = cx
            .update_window(window, |_, window, cx| {
                view.update(cx, |view, cx| view.pump(window, cx));
                window.render_frame(cx);
                (
                    window.find_all("inbox-row").len(),
                    window
                        .find_all("inbox-row-machine")
                        .iter()
                        .map(|found| found.label().unwrap_or_default().to_owned())
                        .collect::<Vec<_>>(),
                )
            })
            .expect("window");
        if rows == 2 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert_eq!(rows, 2, "both rows are listed");
    assert_eq!(
        icons,
        ["Runs on a computer"],
        "only the machine-hosted row has the icon, with its label"
    );
}

// ---- the shell: Machines page and the waiting banner --------------------------------------------

const RAISED_MS: u64 = 1_793_138_416_000;

fn stored_session_on(hub: &str) -> Arc<MemoryStore> {
    let grant = SessionGrant::from_json(&json!({
        "token": "sm_session_token",
        "expires_at": 1_900_000_000,
        "user": { "display_name": "Ada", "github_login": "ada" },
        "org": { "name": "Acme" },
        "role": "admin"
    }))
    .expect("grant");
    let store = Arc::new(MemoryStore::default());
    store
        .save(&Session {
            hub: hub.to_owned(),
            grant,
        })
        .expect("save");
    store
}

fn event_frame(seq: u64, event: Value) -> String {
    json!({
        "type": "event",
        "envelope": {
            "v": 1, "seq": seq, "thread_id": "th_a", "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:16.000Z", "event": event,
        }
    })
    .to_string()
}

/// `th_a` runs on `mch_laptop` and has `tk_q` queued behind its task.
fn thread_a_frames() -> Vec<String> {
    vec![
        event_frame(
            1,
            json!({ "type": "StateSnapshot", "scope": "thread",
                    "snapshot": { "pending_tasks": [{ "task_id": "tk_q", "preview": "then add tests" }] } }),
        ),
        event_frame(
            2,
            json!({ "type": "RunStarted", "reason": "prompt", "agent_id": "agt_claude",
                    "machine_id": "mch_laptop" }),
        ),
    ]
}

fn open_shell(cx: &mut TestAppContext, root: &str) -> (AnyWindowHandle, Entity<AppShell>) {
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::shell::init(cx);
    });
    let hub = root.to_owned();
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1100.), px(800.)),
        };
        gpui_kit::open_window(
            WindowOptions {
                window_bounds: Some(WindowBounds::Windowed(bounds)),
                ..Default::default()
            },
            cx,
            move |window, cx| {
                cx.new(|cx| {
                    AppShell::new(
                        AppConfig {
                            hub: hub.clone(),
                            browser: Arc::new(RecordingBrowser::new()),
                            store: stored_session_on(&hub),
                            prefs: Preferences::default(),
                            prefs_path: None,
                            poll_speedup: 200,
                            link_thread: String::new(),
                            clock: Arc::new(ManualClock::new(RAISED_MS)),
                        },
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("open window")
    })
}

/// Advances the clock in small steps until the label of `id` satisfies `ok`.
fn pump_for_label(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    id: &'static str,
    ok: impl Fn(Option<&str>) -> bool,
) -> Option<String> {
    let mut last = None;
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        last = label(cx, window, id);
        if ok(last.as_deref()) {
            return last;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never matched; last label {last:?}");
}

#[gpui_kit::test]
fn the_task_view_says_waiting_for_an_offline_machine_and_lists_its_queue(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![laptop("offline")]);
    hub.frames
        .lock()
        .expect("lock")
        .insert("th_a".to_owned(), thread_a_frames());
    let (window, view) = open_shell(cx, &root);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| {
            shell.open_thread("th_a", Some("/work/repo"), cx)
        });
    })
    .expect("window");

    let banner = pump_for_label(cx, window, "thread-waiting", |l| l.is_some());
    assert_eq!(
        banner.as_deref(),
        Some("Waiting for Laptop, which is offline. 1 queued.")
    );
    let items = cx
        .update_window(window, |_, window, _| {
            window
                .find_all(WAITING_ITEM_ID)
                .iter()
                .map(|found| found.label().unwrap_or_default().to_owned())
                .collect::<Vec<_>>()
        })
        .expect("window");
    assert_eq!(items, ["Queued for Laptop: then add tests"]);

    // The machine comes back: the banner goes away on the next machines refresh.
    hub.machines.lock().expect("lock")[0]["presence"] = json!({ "state": "online" });
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.refresh_machines(cx));
    })
    .expect("window");
    pump_for_label(cx, window, "thread-waiting", |l| l.is_none());
}

#[gpui_kit::test]
fn the_machines_page_renames_through_the_shell_and_the_feed_follows(cx: &mut TestAppContext) {
    let (hub, root) = hub_with(vec![laptop("online")]);
    let (window, view) = open_shell(cx, &root);
    cx.update_window(window, |_, window, cx| window.click("nav-machines", cx))
        .expect("window");
    pump_for_label(cx, window, "machine-row-mch_laptop", |l| {
        l.is_some_and(|l| l.starts_with("Laptop, Computer, Online"))
    });

    cx.update_window(window, |_, window, cx| {
        window.click("machine-rename-mch_laptop", cx);
        window.render_frame(cx);
        let page = view.read(cx).machines_page().clone();
        page.read(cx).name_input().clone().update(cx, |input, cx| {
            input.set_value("Desk", window, cx);
        });
        window.click("machines-rename-save", cx);
    })
    .expect("window");
    // The PATCH went out and the shell's feed was refetched, so the row shows the new name.
    pump_for_label(cx, window, "machine-row-mch_laptop", |l| {
        l.is_some_and(|l| l.starts_with("Desk, Computer"))
    });
    let writes = hub.writes.lock().expect("lock");
    assert_eq!(writes.len(), 1);
    assert_eq!(writes[0].0, "PATCH");
    assert_eq!(writes[0].3, json!({ "name": "Desk" }));
}
