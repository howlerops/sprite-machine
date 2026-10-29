//! The new-task composer: model logic natively, the view headlessly, sends against a local fake
//! hub (client-api.md §5.1 / §5.2).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use client_gpui::compose::{
    ComposeView, DRAFT_FILE, Invalid, NewTaskModel, Status, Target, execute, fetch_machines,
    parse_machines,
};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};
use serde_json::{Value, json};

// ---- fake hub ----------------------------------------------------------------------------------

#[derive(Default)]
struct Hub {
    posts: Mutex<Vec<(HeaderMap, Value)>>,
    machine_gets: Mutex<Vec<HeaderMap>>,
    /// Replaces [`machines_body`] when set.
    machines: Mutex<Option<Value>>,
}

fn machines_body() -> Value {
    json!({ "machines": [
        { "machine_id": "mch_old", "name": "old-box", "kind": "local",
          "presence": { "state": "offline", "since": "2026-10-27T22:00:00.000Z" },
          "capabilities": { "harnesses": ["claude"], "spawn": ["same_dir"], "max_sessions": 4 } },
        { "machine_id": "mch_laptop", "name": "my-laptop", "kind": "local",
          "presence": { "state": "online", "since": "2026-10-27T22:00:00.000Z" },
          "capabilities": { "harnesses": ["claude", "opencode"], "spawn": ["same_dir", "worktree"], "max_sessions": 32 } },
        { "name": "no id" }
    ]})
}

async fn list_machines(State(hub): State<Arc<Hub>>, headers: HeaderMap) -> Json<Value> {
    hub.machine_gets.lock().expect("lock").push(headers);
    let body = hub.machines.lock().expect("lock").clone();
    Json(body.unwrap_or_else(machines_body))
}

async fn create_thread(
    State(hub): State<Arc<Hub>>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    hub.posts.lock().expect("lock").push((headers, body));
    (
        StatusCode::CREATED,
        Json(json!({ "thread_id": "th_01JBX2", "task_id": "tk_x", "status": "started" })),
    )
}

/// Returns the `…/v1` base URL.
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
                .route("/v1/threads", post(create_thread))
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
    format!("http://127.0.0.1:{}/v1", receiver.recv().expect("hub port"))
}

fn temp_dir(name: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "client-gpui-compose-{name}-{}-{:08x}",
        std::process::id(),
        rand_suffix()
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

fn rand_suffix() -> u32 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.subsec_nanos())
        .unwrap_or(0)
}

fn ready_model() -> NewTaskModel {
    let mut model = NewTaskModel::open(None);
    model.set_machines(parse_machines(&machines_body()));
    model.set_workspace_path("/Users/me/src/app");
    model.set_prompt("Upgrade axum to 0.8");
    model
}

// ---- model --------------------------------------------------------------------------------------

#[test]
fn machines_parse_and_the_first_online_machine_is_chosen() {
    let machines = parse_machines(&machines_body());
    assert_eq!(machines.len(), 2, "the entry without machine_id is skipped");
    assert!(!machines[0].online);
    assert!(machines[1].online);
    assert_eq!(machines[1].harnesses, ["claude", "opencode"]);

    let mut model = NewTaskModel::open(None);
    assert_eq!(model.agents().len(), 6, "fixed list before machines load");
    model.set_machines(machines);
    assert_eq!(model.draft().machine_id.as_deref(), Some("mch_laptop"));
    assert_eq!(model.agents(), ["claude", "opencode"]);
    assert_eq!(model.spawns(), ["same_dir", "worktree"]);
    model.set_agent("goose");
    assert_eq!(
        model.draft().agent,
        "claude",
        "agent the machine lacks is refused"
    );
    model.set_agent("opencode");
    assert_eq!(model.draft().agent, "opencode");
    model.set_spawn("worktree");
    // Switching to a machine that only does same_dir and claude pulls the choices back in line.
    model.set_machine("mch_old");
    assert_eq!(model.draft().agent, "claude");
    assert_eq!(model.draft().spawn, "same_dir");
}

fn capped_machines(local: &str) -> Value {
    json!({ "machines": [
        { "machine_id": "mch_capped", "name": "capped-box", "kind": "local",
          "presence": { "state": "online", "since": "2026-10-27T22:00:00.000Z" },
          "capabilities": { "harnesses": ["claude"], "spawn": ["same_dir"], "max_sessions": 4,
                            "permission_mode": local } },
        { "machine_id": "mch_open", "name": "open-box", "kind": "local",
          "presence": { "state": "online", "since": "2026-10-27T22:00:00.000Z" },
          "capabilities": { "harnesses": ["claude"], "spawn": ["same_dir"], "max_sessions": 4 } },
    ]})
}

#[test]
fn modes_looser_than_the_machines_local_mode_are_not_offered() {
    let mut model = NewTaskModel::open(None);
    model.set_machines(parse_machines(&capped_machines("default")));
    assert_eq!(model.local_mode(), Some("default"));
    assert_eq!(model.modes(), ["default", "plan"]);
    model.set_mode(Some("bypassPermissions"));
    model.set_mode(Some("acceptEdits"));
    assert_eq!(model.draft().mode, None, "a looser mode is refused");
    model.set_mode(Some("plan"));
    assert_eq!(model.draft().mode.as_deref(), Some("plan"));

    model.set_machines(parse_machines(&capped_machines("plan")));
    assert_eq!(model.modes(), ["plan"]);
    model.set_machines(parse_machines(&capped_machines("acceptEdits")));
    assert_eq!(model.modes(), ["default", "acceptEdits", "plan"]);
    model.set_machines(parse_machines(&capped_machines("bypassPermissions")));
    assert_eq!(model.modes().len(), 4);
    // A machine that does not say offers everything.
    model.set_machine("mch_open");
    assert_eq!(model.local_mode(), None);
    assert_eq!(model.modes().len(), 4);
}

#[test]
fn a_mode_chosen_before_switching_to_a_stricter_machine_is_lowered_and_says_so() {
    let mut model = NewTaskModel::open(None);
    model.set_machines(parse_machines(&capped_machines("default")));
    model.set_machine("mch_open");
    model.set_mode(Some("bypassPermissions"));
    assert_eq!(model.capped_from(), None);
    model.set_machine("mch_capped");
    assert_eq!(model.draft().mode.as_deref(), Some("default"));
    assert_eq!(model.capped_from(), Some("bypassPermissions"));
    model.set_workspace_path("/w");
    model.set_prompt("go");
    let sent = model.prepare_send().expect("valid");
    assert_eq!(sent.request.permission_mode.as_deref(), Some("default"));
    // Picking a mode again clears the note.
    model.set_mode(Some("plan"));
    assert_eq!(model.capped_from(), None);
}

#[test]
fn validation_names_the_first_problem() {
    let mut model = NewTaskModel::open(None);
    assert_eq!(model.prepare_send().unwrap_err(), Invalid::NoMachine);
    model.set_machines(parse_machines(&machines_body()));
    assert_eq!(model.prepare_send().unwrap_err(), Invalid::NoWorkspace);
    model.set_workspace_path("  /tmp/app ");
    assert_eq!(model.prepare_send().unwrap_err(), Invalid::NoPrompt);
    model.set_prompt("   \n");
    assert_eq!(model.prepare_send().unwrap_err(), Invalid::NoPrompt);
    model.set_prompt("go");
    assert!(model.prepare_send().is_ok());
    model.set_machine("mch_old");
    assert_eq!(model.prepare_send().unwrap_err(), Invalid::MachineOffline);
}

#[test]
fn the_request_body_carries_every_choice() {
    let mut model = ready_model();
    model.set_agent("opencode");
    model.set_spawn("worktree");
    model.set_mode(Some("plan"));
    model.set_title(" Upgrade axum ");
    model.set_prompt("Upgrade axum to 0.8\n");
    let prepared = model.prepare_send().expect("valid");
    let body = serde_json::to_value(&prepared.request).expect("json");
    let prompt = body["prompt"].clone();
    assert!(prompt["task_id"].as_str().expect("task").starts_with("tk_"));
    assert!(
        prompt["message_id"]
            .as_str()
            .expect("msg")
            .starts_with("msg_")
    );
    assert_eq!(
        body,
        json!({
            "machine_id": "mch_laptop",
            "agent": "opencode",
            "workspace": { "path": "/Users/me/src/app", "spawn": "worktree" },
            "permission_mode": "plan",
            "title": "Upgrade axum",
            "prompt": {
                "task_id": prompt["task_id"],
                "message_id": prompt["message_id"],
                "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }]
            }
        })
    );

    // Without a mode or title those fields are absent, not null.
    let mut model = ready_model();
    let body = serde_json::to_value(model.prepare_send().expect("valid").request).expect("json");
    assert!(body.get("permission_mode").is_none());
    assert!(body.get("title").is_none());
}

#[test]
fn a_retry_reuses_the_key_until_the_draft_changes() {
    let mut model = ready_model();
    let first = model.prepare_send().expect("valid");
    assert_eq!(model.prepare_send().expect("valid"), first);
    model.set_prompt("something else");
    let second = model.prepare_send().expect("valid");
    assert_ne!(second.idempotency_key, first.idempotency_key);
}

#[test]
fn drafts_survive_closing_and_reopening() {
    let dir = temp_dir("draft");
    {
        let mut model = NewTaskModel::open(Some(&dir));
        model.set_machines(parse_machines(&machines_body()));
        model.set_agent("opencode");
        model.set_spawn("worktree");
        model.set_mode(Some("acceptEdits"));
        model.set_workspace_path("/work/app");
        model.set_title("T");
        model.set_prompt("half-written thought");
        assert_eq!(model.save_error(), None);
    }
    assert!(dir.join(DRAFT_FILE).exists());
    let mut reopened = NewTaskModel::open(Some(&dir));
    let draft = reopened.draft().clone();
    assert_eq!(draft.prompt, "half-written thought");
    assert_eq!(draft.workspace_path, "/work/app");
    assert_eq!(draft.agent, "opencode");
    assert_eq!(draft.spawn, "worktree");
    assert_eq!(draft.mode.as_deref(), Some("acceptEdits"));
    assert_eq!(draft.title, "T");
    assert_eq!(draft.machine_id.as_deref(), Some("mch_laptop"));
    // The saved machine is kept once the list loads.
    reopened.set_machines(parse_machines(&machines_body()));
    assert_eq!(reopened.draft(), &draft);

    // A sent task drops the prompt but keeps where it ran; emptying the text removes the file.
    reopened.sent();
    assert_eq!(NewTaskModel::open(Some(&dir)).draft().prompt, "");
    assert_eq!(
        NewTaskModel::open(Some(&dir)).draft().workspace_path,
        "/work/app"
    );
    reopened.set_workspace_path("");
    assert!(!dir.join(DRAFT_FILE).exists());

    // A damaged file opens a blank draft rather than failing.
    std::fs::write(dir.join(DRAFT_FILE), "{not json").expect("write");
    assert_eq!(NewTaskModel::open(Some(&dir)).draft().prompt, "");
}

// ---- client-core against the fake hub ------------------------------------------------------------

#[tokio::test]
async fn the_request_reaches_the_hub_with_key_token_and_body() {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let machines = fetch_machines(&api, Some("tok")).await.expect("machines");
    assert_eq!(machines.len(), 2);
    assert_eq!(
        hub.machine_gets.lock().expect("lock")[0]["authorization"],
        "Bearer tok"
    );

    let mut model = ready_model();
    let prepared = model.prepare_send().expect("valid");
    let created = execute(&api, Some("tok"), &prepared)
        .await
        .expect("created");
    assert_eq!(created.thread_id, "th_01JBX2");
    let posts = hub.posts.lock().expect("lock");
    assert_eq!(posts.len(), 1);
    assert_eq!(
        posts[0].0["idempotency-key"],
        prepared.idempotency_key.as_str()
    );
    assert_eq!(posts[0].0["authorization"], "Bearer tok");
    assert_eq!(posts[0].1["workspace"]["path"], "/Users/me/src/app");
    assert_eq!(
        posts[0].1["prompt"]["parts"],
        json!([{ "type": "text", "text": "Upgrade axum to 0.8" }])
    );
}

// ---- strings -----------------------------------------------------------------------------------

#[test]
fn every_string_id_the_composer_uses_is_in_the_catalog() {
    let source = std::fs::read_to_string(
        PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("src/compose/mod.rs"),
    )
    .expect("read mod.rs");
    let catalog = Catalog::from_source("en", client_gpui::compose::EN_FTL).expect("parses");
    let mut ids = HashSet::new();
    for call in ["tr(\"", "tr_args(\""] {
        let mut rest = source.as_str();
        while let Some(at) = rest.find(call) {
            rest = &rest[at + call.len()..];
            if let Some(end) = rest.find('"') {
                ids.insert(rest[..end].to_owned());
            }
        }
    }
    // Ids picked by match arm or built from a value.
    for line in source.lines() {
        if line.contains(".id(") || line.contains("Button::new(") {
            continue;
        }
        if let Some(start) = line.find("\"compose-") {
            let rest = &line[start + 1..];
            if let Some(end) = rest.find('"') {
                ids.insert(rest[..end].to_owned());
            }
        }
    }
    for spawn in client_gpui::compose::SPAWNS {
        ids.insert(format!("compose-spawn-{spawn}"));
    }
    for mode in client_gpui::compose::MODES {
        ids.insert(format!("compose-mode-{mode}"));
    }
    ids.retain(|id| id.starts_with("compose-") && !id.ends_with('-') && !id.contains('{'));
    assert!(ids.len() > 30, "scan found only {} ids", ids.len());
    let missing: Vec<_> = ids.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from compose.ftl: {missing:?}"
    );
}

// ---- the view, headless --------------------------------------------------------------------------

fn open_composer(
    cx: &mut TestAppContext,
    dir: Option<PathBuf>,
) -> (AnyWindowHandle, Entity<ComposeView>) {
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
            move |window, cx| cx.new(|cx| ComposeView::new(dir, window, cx)),
        )
        .expect("open window")
    })
}

/// Let background work finish: real time, since the fake hub is a local thread.
fn wait_until(
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

#[gpui_kit::test]
fn the_view_offers_only_modes_the_machine_allows_and_says_when_it_capped(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    *hub.machines.lock().expect("lock") = Some(capped_machines("default"));
    let api = spawn_hub(hub.clone());
    let (window, view) = open_composer(cx, None);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api: api.clone(),
                    token: None,
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    wait_until(cx, window, &view, |window, cx| {
        view.read(cx).model().selected_machine().is_some() && window.find("mode-plan").visible()
    });
    cx.update_window(window, |_, window, cx| {
        assert!(window.try_find("mode-default").is_some());
        assert!(window.try_find("mode-acceptEdits").is_none());
        assert!(window.try_find("mode-bypassPermissions").is_none());
        assert!(
            window.try_find("compose-mode-note").is_some(),
            "the cap is explained"
        );
        // Switching to a machine without a cap brings the loose modes back.
        window.click("machine-mch_open", cx);
        window.render_frame(cx);
        assert!(window.find("mode-bypassPermissions").visible());
        assert!(window.try_find("compose-mode-note").is_none());
        // A loose choice carried to the capped machine is lowered, and the note says so.
        window.click("mode-bypassPermissions", cx);
        window.click("machine-mch_capped", cx);
        window.render_frame(cx);
        assert!(window.try_find("mode-bypassPermissions").is_none());
        let note = window.find("compose-mode-note").label().map(str::to_owned);
        assert!(
            note.as_deref()
                .is_some_and(|n| n.contains("looser than this machine allows")),
            "{note:?}"
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn ctrl_enter_sends_the_typed_task_to_the_hub(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let dir = temp_dir("view");
    let (window, view) = open_composer(cx, Some(dir.clone()));
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api: api.clone(),
                    token: Some("tok".to_owned()),
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    // The machines list loads and the online machine is chosen; its agents are the buttons.
    {
        wait_until(cx, window, &view, |window, cx| {
            view.read(cx).model().selected_machine().is_some()
                && window.find("agent-opencode").visible()
        });
    }
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("machine-mch_old").label(),
            Some("Run on old-box")
        );
        assert_eq!(
            window.find("machine-mch_laptop").label(),
            Some("Run on my-laptop, selected")
        );
        window.click("agent-opencode", cx);
        window.click("spawn-worktree", cx);
        window.click("mode-plan", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("agent-opencode").label(),
            Some("Use agent opencode, selected")
        );
        let workspace = view.read(cx).workspace_input().focus_handle(cx);
        window.focus(&workspace, cx);
        window.input("/Users/me/src/app", cx);
        let prompt = view.read(cx).prompt_input().focus_handle(cx);
        window.focus(&prompt, cx);
        window.input("Upgrade axum to 0.8", cx);
        window.render_frame(cx);
    })
    .expect("window");
    // Change events reach the model between updates.
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            view.read(cx).model().draft().prompt,
            "Upgrade axum to 0.8",
            "typing reaches the model"
        );
        // Nothing is sent by typing; the shortcut sends.
        assert!(hub.posts.lock().expect("lock").is_empty());
        window.press(shortcut(), cx);
    })
    .expect("window");
    {
        wait_until(cx, window, &view, |_, cx| {
            matches!(view.read(cx).status(), Status::Sent { .. })
        });
    }
    let posts = hub.posts.lock().expect("lock");
    assert_eq!(posts.len(), 1, "one POST");
    let (headers, body) = &posts[0];
    assert_eq!(headers["authorization"], "Bearer tok");
    assert!(headers.contains_key("idempotency-key"));
    assert_eq!(body["machine_id"], "mch_laptop");
    assert_eq!(body["agent"], "opencode");
    assert_eq!(
        body["workspace"],
        json!({ "path": "/Users/me/src/app", "spawn": "worktree" })
    );
    assert_eq!(body["permission_mode"], "plan");
    assert_eq!(
        body["prompt"]["parts"],
        json!([{ "type": "text", "text": "Upgrade axum to 0.8" }]),
        "the shortcut must not leave a newline in the prompt"
    );
    drop(posts);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("compose-status").label(),
            Some("Started task th_01JBX2.")
        );
        assert_eq!(view.read(cx).model().draft().prompt, "");
    })
    .expect("window");
}

#[cfg(target_os = "macos")]
fn shortcut() -> &'static str {
    "cmd-enter"
}

#[cfg(not(target_os = "macos"))]
fn shortcut() -> &'static str {
    "ctrl-enter"
}

#[gpui_kit::test]
fn the_send_button_and_the_shortcut_refuse_an_incomplete_task(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let (window, view) = open_composer(cx, None);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api: api.clone(),
                    token: None,
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
    {
        wait_until(cx, window, &view, |_, cx| {
            view.read(cx).model().selected_machine().is_some()
        });
    }
    cx.update_window(window, |_, window, cx| {
        window.click("compose-send", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("compose-status").label(),
            Some("Enter the workspace folder.")
        );
    })
    .expect("window");
    assert!(hub.posts.lock().expect("lock").is_empty());
}

#[gpui_kit::test]
fn the_draft_comes_back_when_the_composer_is_reopened(cx: &mut TestAppContext) {
    let dir = temp_dir("reopen");
    let (window, view) = open_composer(cx, Some(dir.clone()));
    cx.update_window(window, |_, window, cx| {
        let workspace = view.read(cx).workspace_input().focus_handle(cx);
        window.focus(&workspace, cx);
        window.input("/work/app", cx);
        let prompt = view.read(cx).prompt_input().focus_handle(cx);
        window.focus(&prompt, cx);
        window.input("unfinished idea", cx);
        window.render_frame(cx);
    })
    .expect("window");
    // Close the window, then open a new composer on the same config directory.
    cx.update_window(window, |_, window, _| window.remove_window())
        .expect("window");
    cx.run_until_parked();
    let (window, view) = open_composer(cx, Some(dir));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let draft = view.read(cx).model().draft().clone();
        assert_eq!(draft.prompt, "unfinished idea");
        assert_eq!(draft.workspace_path, "/work/app");
        assert_eq!(
            view.read(cx).prompt_input().read(cx).value().as_ref(),
            "unfinished idea"
        );
        assert_eq!(
            view.read(cx).workspace_input().read(cx).value().as_ref(),
            "/work/app"
        );
    })
    .expect("window");
}
