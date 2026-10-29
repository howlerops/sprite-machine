//! M3-003: onboarding and empty states. The shell on GPUI's headless platform against a tiny fake
//! hub whose machines list and thread list a test can change; the Fluent catalog; and one test
//! per empty state (what it says and what its one primary action does).

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::get;
use axum::{Json, Router};
use client_core::outbox::ManualClock;
use client_gpui::inbox::Filters;
use client_gpui::onboarding::{
    BYO_ID, BYO_REASON_ID, COPY_RC_ID, EXPECT_ID, LOGIN_COMMAND_ID, MODEL_KEY_ID,
    MODEL_KEY_REASON_ID, RC_COMMAND_ID, SKIP_ID, VIEW_ID, WAITING_ID,
};
use client_gpui::shell::auth::SessionGrant;
use client_gpui::shell::browser::RecordingBrowser;
use client_gpui::shell::i18n::Catalog;
use client_gpui::shell::nav::Section;
use client_gpui::shell::prefs::Preferences;
use client_gpui::shell::session::{MemoryStore, Session, SessionStore};
use client_gpui::shell::{AppConfig, AppShell};
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Point, TestAppContext,
    WindowBounds, WindowOptions, px, size,
};
use serde_json::{Value, json};

// ---- a fake hub -------------------------------------------------------------------------------

#[derive(Default)]
struct Hub {
    machines: Mutex<Vec<Value>>,
    threads: Mutex<Vec<Value>>,
    /// The status `GET /v1/machines` answers (0 means 200).
    machines_status: AtomicUsize,
}

async fn list_machines(State(hub): State<Arc<Hub>>) -> Response {
    let status = hub.machines_status.load(Ordering::SeqCst);
    if status != 0 {
        return StatusCode::from_u16(status as u16)
            .unwrap_or(StatusCode::INTERNAL_SERVER_ERROR)
            .into_response();
    }
    Json(json!({ "machines": hub.machines.lock().expect("lock").clone() })).into_response()
}

async fn list_threads(State(hub): State<Arc<Hub>>) -> Json<Value> {
    let threads = hub.threads.lock().expect("lock").clone();
    Json(json!({ "threads": threads, "next_cursor": null }))
}

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
                .route("/v1/threads", get(list_threads))
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

fn laptop() -> Value {
    json!({
        "machine_id": "mch_laptop", "name": "Laptop",
        "presence": { "state": "online" },
        "capabilities": { "harnesses": ["claude"], "features": [] },
    })
}

fn signed_in(hub: &str) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::default());
    let grant = SessionGrant::from_json(&json!({
        "token": "sm_session_token", "expires_at": 1_900_000_000,
        "user": { "display_name": "Ada" }, "org": { "name": "Acme" }, "role": "member"
    }))
    .expect("grant");
    store
        .save(&Session {
            hub: hub.to_owned(),
            grant,
        })
        .expect("save");
    store
}

fn open_shell(
    cx: &mut TestAppContext,
    hub: &Arc<Hub>,
) -> (AnyWindowHandle, Entity<AppShell>, String) {
    let url = spawn_hub(hub.clone());
    cx.executor().allow_parking();
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::shell::init(cx);
    });
    let store = signed_in(&url);
    let hub_url = url.clone();
    let (window, view) = cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(900.)),
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
                            hub: hub_url,
                            browser: Arc::new(RecordingBrowser::new()),
                            store,
                            prefs: Preferences::default(),
                            prefs_path: None,
                            poll_speedup: 200,
                            link_thread: String::new(),
                            clock: Arc::new(ManualClock::new(1_793_138_416_000)),
                        },
                        window,
                        cx,
                    )
                })
            },
        )
        .expect("open window")
    });
    (window, view, url)
}

/// Advances the test clock in small steps (the shell's timers and fetches run on it) until
/// `done` holds on a freshly drawn frame.
fn pump_until(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    what: &str,
    done: impl Fn(&mut gpui_kit::Window, &mut gpui_kit::App) -> bool,
) {
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        let finished = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                done(window, cx)
            })
            .expect("window");
        if finished {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

fn wait_shown(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    pump_until(cx, window, id, |window, _| window.try_find(id).is_some());
}

fn wait_gone(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    pump_until(cx, window, id, |window, _| window.try_find(id).is_none());
}

fn label(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> Option<String> {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find(id)
            .and_then(|found| found.label().map(str::to_owned))
    })
    .expect("window")
}

fn click(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) {
    cx.update_window(window, |_, window, cx| window.click(id, cx))
        .expect("window");
    cx.run_until_parked();
}

fn shown(cx: &mut TestAppContext, window: AnyWindowHandle, id: &'static str) -> bool {
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.try_find(id).is_some()
    })
    .expect("window")
}

fn section(cx: &mut TestAppContext, view: &Entity<AppShell>) -> Section {
    cx.update(|cx| view.read(cx).model().nav.current())
}

// ---- path (c): sm rc ---------------------------------------------------------------------------

#[gpui_kit::test]
fn with_no_machines_onboarding_shows_the_sm_rc_steps_with_exact_commands(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, _view, url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    assert_eq!(
        label(cx, window, LOGIN_COMMAND_ID).as_deref(),
        Some(format!("sm login --hub {url}").as_str())
    );
    assert_eq!(label(cx, window, RC_COMMAND_ID).as_deref(), Some("sm rc"));
    let expect = label(cx, window, EXPECT_ID).expect("what to expect");
    assert!(
        expect.contains("QR code") && expect.contains("session:"),
        "{expect}"
    );
    assert_eq!(
        label(cx, window, WAITING_ID).as_deref(),
        Some("Waiting for a computer to connect...")
    );
    // The inbox list is not what the page shows.
    assert!(!shown(cx, window, "inbox"));
}

#[gpui_kit::test]
fn the_command_boxes_copy_their_text(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, _view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    click(cx, window, COPY_RC_ID);
    let copied = cx.update(|cx| cx.read_from_clipboard().and_then(|item| item.text()));
    assert_eq!(copied.as_deref(), Some("sm rc"));
    pump_until(cx, window, "Copied", |window, _| {
        window.find(COPY_RC_ID).label() == Some("Copy the sm rc command")
    });
}

#[gpui_kit::test]
fn when_the_machines_feed_reports_a_machine_onboarding_completes_and_the_inbox_shows(
    cx: &mut TestAppContext,
) {
    let hub = Arc::new(Hub::default());
    let (window, view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    hub.machines.lock().expect("lock").push(laptop());
    // The machines list is refreshed on a timer; the test clock steps over it.
    cx.executor()
        .advance_clock(client_gpui::shell::machines::REFRESH_EVERY);
    wait_gone(cx, window, VIEW_ID);
    wait_shown(cx, window, "inbox");
    assert_eq!(section(cx, &view), Section::Inbox);
    // Done for good: the machine going away again does not bring the page back.
    hub.machines.lock().expect("lock").clear();
    cx.executor()
        .advance_clock(client_gpui::shell::machines::REFRESH_EVERY);
    for _ in 0..20 {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!shown(cx, window, VIEW_ID));
}

#[gpui_kit::test]
fn an_unreachable_machines_feed_is_not_mistaken_for_no_machines(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    hub.machines_status.store(500, Ordering::SeqCst);
    let (window, view, _url) = open_shell(cx, &hub);
    pump_until(cx, window, "the inbox", |window, _| {
        window.try_find("inbox").is_some()
    });
    for _ in 0..20 {
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        std::thread::sleep(Duration::from_millis(10));
    }
    assert!(!shown(cx, window, VIEW_ID));
    assert!(!cx.update(|cx| view.read(cx).onboarding_visible(cx)));
}

#[gpui_kit::test]
fn a_user_with_threads_and_no_machine_goes_straight_to_the_inbox(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    hub.threads.lock().expect("lock").push(json!({
        "thread_id": "th_cloud", "title": "Cloud task", "machine_id": "mch_cloud",
        "agent": "claude", "status": "done", "updated_at": "2026-10-28T10:00:00.000Z",
        "last_seq": 3, "workspace": "/work/cloud",
    }));
    let (window, _view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, "inbox-row");
    assert!(!shown(cx, window, VIEW_ID));
}

#[gpui_kit::test]
fn skipping_leaves_onboarding_for_the_inbox_empty_state(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, _view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    click(cx, window, SKIP_ID);
    wait_gone(cx, window, VIEW_ID);
    wait_shown(cx, window, "inbox-empty-action-new");
}

// ---- paths (a) and (b): shown, disabled, with reasons -----------------------------------------

#[gpui_kit::test]
fn the_model_key_and_byo_url_paths_are_shown_disabled_with_their_reasons(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, _view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    let key = label(cx, window, MODEL_KEY_REASON_ID).expect("model key reason");
    assert!(
        key.starts_with("Not available yet") && key.contains("M2-002"),
        "{key}"
    );
    let byo = label(cx, window, BYO_REASON_ID).expect("byo reason");
    assert!(
        byo.starts_with("Not available yet") && byo.contains("M2-004"),
        "{byo}"
    );
    assert_eq!(
        label(cx, window, MODEL_KEY_ID).as_deref(),
        Some("Add a model API key, not available yet")
    );
    assert_eq!(
        label(cx, window, BYO_ID).as_deref(),
        Some("Add a bring-your-own A2A URL, not available yet")
    );
    // Clicking a disabled path changes nothing.
    click(cx, window, MODEL_KEY_ID);
    click(cx, window, BYO_ID);
    assert!(shown(cx, window, VIEW_ID));
    let paths = client_gpui::onboarding::paths();
    assert!(!paths[0].enabled() && !paths[1].enabled() && paths[2].enabled());
}

// ---- empty states: one primary action each ----------------------------------------------------

/// A shell past onboarding: the hub has a machine, so every page shows its own empty state.
fn open_past_onboarding(cx: &mut TestAppContext) -> (Arc<Hub>, AnyWindowHandle, Entity<AppShell>) {
    let hub = Arc::new(Hub::default());
    hub.machines.lock().expect("lock").push(laptop());
    let (window, view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, "inbox-empty-action-new");
    (hub, window, view)
}

#[gpui_kit::test]
fn empty_inbox_says_so_and_new_task_opens_the_composer(cx: &mut TestAppContext) {
    let (_hub, window, view) = open_past_onboarding(cx);
    assert_eq!(
        label(cx, window, "inbox-status").as_deref(),
        Some("No tasks yet. Start one with New task.")
    );
    assert_eq!(
        label(cx, window, "inbox-empty-action-new").as_deref(),
        Some("Start a new task")
    );
    click(cx, window, "inbox-empty-action-new");
    assert_eq!(section(cx, &view), Section::NewTask);
}

#[gpui_kit::test]
fn an_inbox_emptied_by_filters_offers_clear_filters(cx: &mut TestAppContext) {
    let (_hub, window, view) = open_past_onboarding(cx);
    click(cx, window, "inbox-status-failed");
    wait_shown(cx, window, "inbox-empty-action-clear");
    assert_eq!(
        label(cx, window, "inbox-status").as_deref(),
        Some("No tasks match.")
    );
    assert!(!shown(cx, window, "inbox-empty-action-new"));
    click(cx, window, "inbox-empty-action-clear");
    wait_shown(cx, window, "inbox-empty-action-new");
    let filters = cx.update(|cx| view.read(cx).inbox().read(cx).model().filters().clone());
    assert_eq!(filters, Filters::default());
}

#[gpui_kit::test]
fn an_empty_agents_table_offers_to_connect_a_machine(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    click(cx, window, SKIP_ID);
    wait_gone(cx, window, VIEW_ID);
    click(cx, window, "nav-agents");
    wait_shown(cx, window, "no-agents-action");
    assert_eq!(
        label(cx, window, "no-agents-action").as_deref(),
        Some("Show how to connect a machine")
    );
    click(cx, window, "no-agents-action");
    wait_shown(cx, window, VIEW_ID);
    assert_eq!(section(cx, &view), Section::Inbox);
}

#[gpui_kit::test]
fn a_new_task_with_no_machines_offers_to_connect_one(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let (window, view, _url) = open_shell(cx, &hub);
    wait_shown(cx, window, VIEW_ID);
    click(cx, window, SKIP_ID);
    wait_gone(cx, window, VIEW_ID);
    click(cx, window, "nav-new-task");
    wait_shown(cx, window, "no-machines-action");
    click(cx, window, "no-machines-action");
    wait_shown(cx, window, VIEW_ID);
    assert_eq!(section(cx, &view), Section::Inbox);
}

#[gpui_kit::test]
fn an_empty_thread_offers_to_write_the_first_message(cx: &mut TestAppContext) {
    let (_hub, window, view) = open_past_onboarding(cx);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.open_thread("th_empty", None, cx));
    })
    .expect("window");
    wait_shown(cx, window, "empty-thread");
    assert_eq!(
        label(cx, window, "empty-thread").as_deref(),
        Some("Nothing here yet. The agent's messages and tool calls appear as they arrive.")
    );
    click(cx, window, "empty-thread-write");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let prompt = view.read(cx).composer().read(cx).prompt_input().clone();
        assert!(
            prompt.focus_handle(cx).is_focused(window),
            "the composer's prompt has the cursor"
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn a_palette_with_no_match_offers_to_clear_the_search(cx: &mut TestAppContext) {
    let (_hub, window, view) = open_past_onboarding(cx);
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| shell.toggle_palette(window, cx));
        window.render_frame(cx);
        window.input("zzzzqqqq", cx);
        window.render_frame(cx);
    })
    .expect("window");
    cx.run_until_parked();
    wait_shown(cx, window, "palette-empty-action");
    assert_eq!(
        label(cx, window, "palette-empty").as_deref(),
        Some("No matching commands")
    );
    click(cx, window, "palette-empty-action");
    wait_gone(cx, window, "palette-empty");
    let query = cx.update(|cx| {
        let palette = view.read(cx).palette().clone();
        palette.read(cx).input().read(cx).value().to_string()
    });
    assert_eq!(query, "");
    assert!(shown(cx, window, "palette-item-0"));
}

// ---- strings ----------------------------------------------------------------------------------

fn literals(file: &str) -> Vec<String> {
    let path: PathBuf = Path::new(env!("CARGO_MANIFEST_DIR")).join(file);
    let source = std::fs::read_to_string(path).expect("source");
    source
        .split('"')
        .skip(1)
        .step_by(2)
        .map(str::to_owned)
        .collect()
}

#[test]
fn every_onboarding_string_id_exists_and_is_used() {
    let catalog =
        Catalog::from_source("en", client_gpui::onboarding::EN_FTL).expect("onboarding.ftl parses");
    let mut used = BTreeSet::new();
    for file in ["src/onboarding/view.rs", "src/onboarding/model.rs"] {
        for literal in literals(file) {
            if literal.starts_with("onboarding-")
                && literal
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                used.insert(literal);
            }
        }
    }
    // Element ids share the prefix; they are not strings.
    let elements = [
        VIEW_ID,
        MODEL_KEY_ID,
        MODEL_KEY_REASON_ID,
        BYO_ID,
        BYO_REASON_ID,
        LOGIN_COMMAND_ID,
        RC_COMMAND_ID,
        EXPECT_ID,
        client_gpui::onboarding::COPY_LOGIN_ID,
        COPY_RC_ID,
        WAITING_ID,
        SKIP_ID,
    ];
    let defined: BTreeSet<String> = Catalog::ids(client_gpui::onboarding::EN_FTL)
        .into_iter()
        .collect();
    let strings: BTreeSet<String> = used
        .into_iter()
        .filter(|id| !elements.contains(&id.as_str()) || defined.contains(id))
        .collect();
    assert!(strings.len() > 15, "scan found {} ids", strings.len());
    for id in &strings {
        assert!(catalog.has(id), "{id} missing from onboarding.ftl");
        assert!(catalog.format(id, &[]).is_some(), "{id} does not format");
    }
    // `onboarding-waiting` is both a string and (via WAITING_ID) an element id; the rest of the
    // catalog is used too.
    let dead: Vec<_> = defined.difference(&strings).collect();
    assert!(dead.is_empty(), "onboarding.ftl ids no code uses: {dead:?}");
}

#[test]
fn the_empty_state_actions_have_their_strings_in_each_catalog() {
    let ids = [
        (client_gpui::inbox::EN_FTL, "inbox-empty-new-task"),
        (client_gpui::inbox::EN_FTL, "inbox-empty-new-task-a11y"),
        (client_gpui::inbox::EN_FTL, "inbox-empty-clear-filters"),
        (client_gpui::inbox::EN_FTL, "inbox-empty-clear-filters-a11y"),
        (client_gpui::agents::EN_FTL, "agents-empty-setup"),
        (client_gpui::agents::EN_FTL, "agents-empty-setup-a11y"),
        (client_gpui::compose::EN_FTL, "compose-machines-empty-setup"),
        (
            client_gpui::compose::EN_FTL,
            "compose-machines-empty-setup-a11y",
        ),
        (client_gpui::palette::EN_FTL, "palette-empty-clear"),
        (client_gpui::palette::EN_FTL, "palette-empty-clear-a11y"),
        (client_gpui::shell::i18n::EN_FTL, "transcript-empty"),
        (client_gpui::shell::i18n::EN_FTL, "transcript-empty-write"),
        (
            client_gpui::shell::i18n::EN_FTL,
            "transcript-empty-write-hint",
        ),
    ];
    for (source, id) in ids {
        let catalog = Catalog::from_source("en", source).expect("parses");
        assert!(catalog.format(id, &[]).is_some(), "{id} missing or broken");
    }
}
