//! The desktop inbox: the model natively, the requests and the view against a local fake hub
//! (client-api.md §5.2, §5.4).

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::{Path, RawQuery, State};
use axum::http::{HeaderMap, StatusCode};
use axum::response::IntoResponse;
use axum::routing::{get, post};
use axum::{Json, Router};
use client_gpui::inbox::{
    Clock, Failure, InboxEvent, InboxModel, InboxView, InterruptKind, Item, SEARCH_DEBOUNCE,
    Section, Status, Target, Verdict, answer_interrupt, fetch_threads, parse_threads, patch_thread,
};
use client_gpui::shell::i18n::Catalog;
use gpui_kit::test::TestWindowExt;
use gpui_kit::{
    AnyWindowHandle, AppContext as _, Bounds, Entity, Focusable as _, Point, ScrollDelta,
    Subscription, TestAppContext, WindowBounds, WindowOptions, point, px, size,
};
use serde_json::{Value, json};

// ---- fake hub ----------------------------------------------------------------------------------

#[derive(Default)]
struct Hub {
    /// Decoded query pairs of every `GET /v1/threads`.
    list_gets: Mutex<Vec<Vec<(String, String)>>>,
    threads: Mutex<Vec<Value>>,
    /// Rows per page; 0 means one page with everything.
    page_size: Mutex<usize>,
    patches: Mutex<Vec<(String, HeaderMap, Value)>>,
    answers: Mutex<Vec<(String, String, HeaderMap, Value)>>,
    patch_status: Mutex<u16>,
    answer_status: Mutex<u16>,
}

fn pairs(raw: Option<String>) -> Vec<(String, String)> {
    url::form_urlencoded::parse(raw.unwrap_or_default().as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

async fn list_threads(State(hub): State<Arc<Hub>>, RawQuery(raw): RawQuery) -> Json<Value> {
    let query = pairs(raw);
    hub.list_gets.lock().expect("lock").push(query.clone());
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.to_owned())
    };
    let mut threads: Vec<Value> = hub
        .threads
        .lock()
        .expect("lock")
        .iter()
        .filter(|t| get("status").is_none_or(|s| t["status"] == s.as_str()))
        .filter(|t| get("agent").is_none_or(|a| t["agent"] == a.as_str()))
        .filter(|t| {
            get("q").is_none_or(|q| t["title"].as_str().is_some_and(|title| title.contains(&q)))
        })
        .cloned()
        .collect();
    let page = *hub.page_size.lock().expect("lock");
    let mut next = Value::Null;
    if page > 0 {
        let from: usize = get("cursor").and_then(|c| c.parse().ok()).unwrap_or(0);
        threads = threads.into_iter().skip(from).collect();
        if threads.len() > page {
            threads.truncate(page);
            next = json!((from + page).to_string());
        }
    }
    Json(json!({ "threads": threads, "next_cursor": next }))
}

async fn patch(
    State(hub): State<Arc<Hub>>,
    Path(id): Path<String>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    hub.patches.lock().expect("lock").push((id, headers, body));
    let status = *hub.patch_status.lock().expect("lock");
    if status >= 400 {
        return (
            StatusCode::from_u16(status).expect("status"),
            Json(json!({ "error": "boom", "message": "the hub said no" })),
        );
    }
    (StatusCode::OK, Json(json!({ "ok": true })))
}

async fn answer(
    State(hub): State<Arc<Hub>>,
    Path((thread, interrupt)): Path<(String, String)>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> impl IntoResponse {
    hub.answers
        .lock()
        .expect("lock")
        .push((thread, interrupt, headers, body));
    let status = *hub.answer_status.lock().expect("lock");
    if status >= 400 {
        return (
            StatusCode::from_u16(status).expect("status"),
            Json(json!({ "error": "already_resolved", "message": "already answered" })),
        );
    }
    (StatusCode::OK, Json(json!({ "interrupt_id": "int_9" })))
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
                .route("/v1/threads", get(list_threads))
                .route("/v1/threads/{id}", axum::routing::patch(patch))
                .route("/v1/threads/{id}/interrupts/{interrupt}", post(answer))
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

fn thread(id: &str, title: &str, status: &str) -> Value {
    json!({ "thread_id": id, "title": title, "machine_id": "mch_1", "agent": "claude",
            "status": status, "updated_at": "2026-10-28T10:00:00.000Z", "last_seq": 7 })
}

/// An open approval whose option ids are deliberately not "allow" / "reject".
fn needing(id: &str, title: &str) -> Value {
    let mut row = thread(id, title, "needs_you");
    row["pending_interrupts"] = json!([{
        "interrupt_id": "int_9", "title": "Allow edit to src/main.rs?",
        "options": [
            { "option_id": "yes_once", "name": "Yes", "kind": "allow_once" },
            { "option_id": "nope", "name": "No", "kind": "reject_once" }
        ]
    }]);
    row
}

fn run_async<T>(future: impl std::future::Future<Output = T>) -> T {
    tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("runtime")
        .block_on(future)
}

struct ManualClock(AtomicU64);

impl ManualClock {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(1_000)))
    }
    fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now(&self) -> Duration {
        Duration::from_millis(self.0.load(Ordering::SeqCst))
    }
}

fn model_with(rows: Vec<Value>) -> (InboxModel, Arc<ManualClock>) {
    let clock = ManualClock::new();
    let mut model = InboxModel::new(clock.clone());
    let request = model.list_request();
    let (rows, _) = parse_threads(&json!({ "threads": rows }));
    assert!(model.apply_list(request.generation, rows));
    (model, clock)
}

fn failure(status: u16) -> Failure {
    Failure {
        status: Some(status),
        message: format!("HTTP {status}"),
    }
}

// ---- model: sections ------------------------------------------------------------------------------

#[test]
fn threads_fall_into_needs_you_running_and_done_in_the_hubs_order() {
    let (model, _) = model_with(vec![
        thread("th_done", "Done one", "done"),
        thread("th_run1", "Run one", "running"),
        needing("th_need", "Needs one"),
        thread("th_idle", "Idle one", "idle"),
        thread("th_run2", "Run two", "running"),
    ]);
    let ids = |section| {
        model
            .section(section)
            .iter()
            .map(|row| row.thread_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(Section::NeedsYou), ["th_need"]);
    assert_eq!(ids(Section::Running), ["th_run1", "th_run2"]);
    assert_eq!(ids(Section::Done), ["th_done", "th_idle"]);
    // The flat list has a header before each non-empty section.
    let items = model.items();
    assert_eq!(items.len(), 3 + 5);
    assert_eq!(
        items[0],
        Item::Header {
            section: Section::NeedsYou,
            count: 1
        }
    );
    assert!(matches!(items[1], Item::Row(_)));
    assert_eq!(
        items[2],
        Item::Header {
            section: Section::Running,
            count: 2
        }
    );
}

#[test]
fn empty_sections_have_no_header() {
    let (model, _) = model_with(vec![thread("th_run", "Run", "running")]);
    assert_eq!(model.items().len(), 2);
}

#[test]
fn rows_parse_the_list_shape_and_ignore_the_unknown() {
    let (rows, cursor) = parse_threads(&json!({
        "threads": [needing("th_a", "A"), { "title": "no id" },
                    { "thread_id": "th_b", "workspace": { "path": "/src/app" }, "archived": true }],
        "next_cursor": "c9"
    }));
    assert_eq!(cursor.as_deref(), Some("c9"));
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0].pending[0].options[0].option_id, "yes_once");
    assert_eq!(rows[1].workspace.as_deref(), Some("/src/app"));
    assert!(rows[1].archived);
}

// ---- model: search and filters ---------------------------------------------------------------------

#[test]
fn search_asks_the_hub_only_after_the_box_is_quiet() {
    let (mut model, clock) = model_with(vec![]);
    model.set_search("f");
    clock.advance(200);
    model.set_search("fi");
    clock.advance(200);
    model.set_search("fix");
    // 200 ms after the last keystroke, 400 ms after the first: still waiting.
    clock.advance(200);
    assert!(!model.poll_search(), "debounce restarts on every keystroke");
    clock.advance(SEARCH_DEBOUNCE.as_millis() as u64 - 200);
    assert!(model.poll_search(), "fires once the box has been quiet");
    assert!(!model.poll_search(), "and only once");
    let request = model.list_request();
    assert_eq!(request.query, [("q".to_owned(), "fix".to_owned())]);
    // Typing and deleting back to the text already used asks for nothing.
    model.set_search("fixx");
    model.set_search("fix");
    clock.advance(1_000);
    assert!(!model.poll_search());
}

#[test]
fn filters_become_query_pairs_and_changes_are_reported() {
    let (mut model, _) = model_with(vec![]);
    assert!(model.set_status_filter(Some(Status::NeedsYou)));
    assert!(!model.set_status_filter(Some(Status::NeedsYou)));
    assert!(model.set_agent_filter(Some("codex")));
    assert!(model.set_workspace_filter("  /src/app "));
    assert!(model.set_show_archived(true));
    let query = model.list_request().query;
    let get = |key: &str| {
        query
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.as_str())
    };
    assert_eq!(get("status"), Some("needs_you"));
    assert_eq!(get("agent"), Some("codex"));
    assert_eq!(get("workspace"), Some("/src/app"));
    assert_eq!(get("archived"), Some("true"));
    assert_eq!(get("q"), None);
}

#[test]
fn a_late_answer_to_an_older_request_is_dropped() {
    let mut model = InboxModel::new(ManualClock::new());
    let old = model.list_request();
    let new = model.list_request();
    let (rows, _) = parse_threads(&json!({ "threads": [thread("th_old", "Old", "done")] }));
    assert!(!model.apply_list(old.generation, rows));
    assert!(model.rows().is_empty());
    let (rows, _) = parse_threads(&json!({ "threads": [thread("th_new", "New", "done")] }));
    assert!(model.apply_list(new.generation, rows));
    assert_eq!(model.rows()[0].thread_id, "th_new");
}

// ---- model: optimistic operations -------------------------------------------------------------------

#[test]
fn rename_shows_at_once_keeps_on_success_and_rolls_back_on_error() {
    let (mut model, _) = model_with(vec![thread("th_a", "Old title", "done")]);
    assert!(model.begin_rename("th_a", "   ").is_none(), "blank");
    assert!(model.begin_rename("th_a", "Old title").is_none(), "same");
    let request = model.begin_rename("th_a", "  New title ").expect("request");
    assert_eq!(request.body, json!({ "title": "New title" }));
    assert_eq!(model.row("th_a").expect("row").title, "New title");
    model.finish(request.op, Err(failure(500)));
    assert_eq!(model.row("th_a").expect("row").title, "Old title");
    assert!(model.error().is_some());

    let request = model.begin_rename("th_a", "Better").expect("request");
    model.finish(request.op, Ok(()));
    assert_eq!(model.row("th_a").expect("row").title, "Better");
    assert!(model.error().is_none());
}

#[test]
fn archive_removes_the_row_at_once_and_restores_it_in_place_on_error() {
    let (mut model, _) = model_with(vec![
        thread("th_1", "One", "done"),
        thread("th_2", "Two", "done"),
        thread("th_3", "Three", "done"),
    ]);
    let request = model.begin_archive("th_2", true).expect("request");
    assert_eq!(request.body, json!({ "archived": true }));
    let ids = |model: &InboxModel| {
        model
            .rows()
            .iter()
            .map(|r| r.thread_id.clone())
            .collect::<Vec<_>>()
    };
    assert_eq!(ids(&model), ["th_1", "th_3"]);
    model.finish(request.op, Err(failure(500)));
    assert_eq!(ids(&model), ["th_1", "th_2", "th_3"], "back where it was");

    let request = model.begin_archive("th_2", true).expect("request");
    model.finish(request.op, Ok(()));
    assert_eq!(ids(&model), ["th_1", "th_3"]);
    // Archived threads can be shown, and restored.
    model.set_show_archived(true);
    assert_eq!(ids(&model), ["th_1", "th_2", "th_3"]);
    assert!(model.row("th_2").expect("row").archived);
    let request = model.begin_archive("th_2", false).expect("restore");
    assert_eq!(request.body, json!({ "archived": false }));
    assert!(!model.row("th_2").expect("row").archived);
    assert!(model.begin_archive("th_nope", true).is_none());
}

#[test]
fn a_quick_answer_uses_the_interrupts_own_option_ids_and_one_key_per_intent() {
    let (mut model, _) = model_with(vec![needing("th_a", "Needs me")]);
    let allow = model.begin_answer("th_a", Verdict::Allow).expect("allow");
    assert_eq!(allow.interrupt_id, "int_9");
    assert_eq!(
        allow.body,
        json!({ "outcome": { "type": "selected", "option_id": "yes_once" } })
    );
    assert!(allow.idempotency_key.starts_with("idk_"));
    // Optimistically the thread has left Needs you; there is nothing left to answer.
    assert_eq!(model.row("th_a").expect("row").status, Status::Running);
    assert!(model.begin_answer("th_a", Verdict::Deny).is_none());
    // A failure puts the approval back; the next try is a new intent with a new key.
    model.finish(allow.op, Err(failure(500)));
    assert_eq!(model.row("th_a").expect("row").status, Status::NeedsYou);
    let deny = model.begin_answer("th_a", Verdict::Deny).expect("deny");
    assert_eq!(
        deny.body,
        json!({ "outcome": { "type": "selected", "option_id": "nope" } })
    );
    assert_ne!(deny.idempotency_key, allow.idempotency_key);
    model.finish(deny.op, Ok(()));
    assert_eq!(model.row("th_a").expect("row").status, Status::Running);
}

#[test]
fn an_approval_someone_else_resolved_is_not_put_back() {
    let (mut model, _) = model_with(vec![needing("th_a", "Needs me")]);
    let request = model.begin_answer("th_a", Verdict::Allow).expect("allow");
    model.finish(request.op, Err(failure(409)));
    let row = model.row("th_a").expect("row");
    assert!(row.pending.is_empty());
    assert!(model.error().is_some());
}

#[test]
fn an_approval_without_allow_or_reject_options_has_no_quick_answer() {
    let mut row = thread("th_f", "Form", "needs_you");
    row["pending_interrupts"] = json!([{ "interrupt_id": "int_f", "title": "Fill in",
        "options": [{ "option_id": "submit", "name": "Submit", "kind": "submit" }] }]);
    let (mut model, _) = model_with(vec![row, thread("th_n", "Plain", "needs_you")]);
    assert!(model.begin_answer("th_f", Verdict::Allow).is_none());
    assert!(model.begin_answer("th_f", Verdict::Deny).is_none());
    assert!(model.begin_answer("th_n", Verdict::Allow).is_none());
}

/// A form (`kind` form, no options) that the hub lists before the approval.
fn form_entry(interrupt_id: &str, title: &str) -> Value {
    json!({ "interrupt_id": interrupt_id, "title": title, "kind": "form", "options": [] })
}

/// An auth entry (`kind` auth, no options).
fn auth_entry(interrupt_id: &str, title: &str) -> Value {
    json!({ "interrupt_id": interrupt_id, "title": title, "kind": "auth", "options": [] })
}

#[test]
fn a_pending_entry_carries_its_kind_and_a_missing_kind_is_an_approval() {
    let mut row = needing("th_m", "Mixed");
    let approval = row["pending_interrupts"][0].clone();
    row["pending_interrupts"] = json!([
        form_entry("int_f", "Fill in"),
        auth_entry("int_a", "Sign in"),
        approval,
        { "interrupt_id": "int_x", "title": "Future", "kind": "widget", "options": [] }
    ]);
    let (model, _) = model_with(vec![row]);
    let kinds: Vec<InterruptKind> = model
        .row("th_m")
        .expect("row")
        .pending
        .iter()
        .map(|p| p.kind)
        .collect();
    assert_eq!(
        kinds,
        vec![
            InterruptKind::Form,
            InterruptKind::Auth,
            InterruptKind::Approval,
            InterruptKind::Other,
        ]
    );
    assert_eq!(InterruptKind::parse(None), InterruptKind::Approval);
}

#[test]
fn a_quick_answer_takes_the_approval_listed_after_a_form() {
    let mut row = needing("th_m", "Mixed");
    let approval = row["pending_interrupts"][0].clone();
    row["pending_interrupts"] = json!([form_entry("int_f", "Fill in"), approval]);
    let (mut model, _) = model_with(vec![row]);
    let allow = model.begin_answer("th_m", Verdict::Allow).expect("allow");
    assert_eq!(allow.interrupt_id, "int_9");
    assert_eq!(
        allow.body,
        json!({ "outcome": { "type": "selected", "option_id": "yes_once" } })
    );
    model.finish(allow.op, Ok(()));
    // The form is still open, so the row stays in Needs you with the form as its detail.
    let row = model.row("th_m").expect("row");
    assert_eq!(row.status, Status::NeedsYou);
    assert_eq!(row.detail(), Some("Fill in"));
    assert!(model.begin_answer("th_m", Verdict::Deny).is_none());
}

#[test]
fn a_row_with_only_a_form_or_auth_has_no_quick_answer_and_its_first_title_is_the_detail() {
    let mut form = thread("th_f", "Form", "needs_you");
    form["pending_interrupts"] = json!([
        form_entry("int_f", "Fill in"),
        auth_entry("int_a", "Sign in")
    ]);
    let mut auth = thread("th_a", "Auth", "needs_you");
    auth["pending_interrupts"] = json!([auth_entry("int_a", "Sign in to GitHub")]);
    let (mut model, _) = model_with(vec![form, auth]);
    for id in ["th_f", "th_a"] {
        assert!(
            model.begin_answer(id, Verdict::Allow).is_none(),
            "{id} allow"
        );
        assert!(model.begin_answer(id, Verdict::Deny).is_none(), "{id} deny");
    }
    assert!(model.row("th_f").expect("row").first_approval().is_none());
    assert_eq!(model.row("th_f").expect("row").detail(), Some("Fill in"));
    assert_eq!(
        model.row("th_a").expect("row").detail(),
        Some("Sign in to GitHub")
    );
}

// ---- requests against the fake hub ---------------------------------------------------------------------

#[test]
fn the_list_request_carries_the_filters_and_follows_the_cursor() {
    let hub = Arc::new(Hub::default());
    *hub.threads.lock().expect("lock") = (0..5)
        .map(|i| thread(&format!("th_{i}"), &format!("Task {i}"), "running"))
        .collect();
    *hub.page_size.lock().expect("lock") = 2;
    let api = spawn_hub(hub.clone());
    let mut model = InboxModel::new(ManualClock::new());
    model.set_status_filter(Some(Status::Running));
    model.set_agent_filter(Some("claude"));
    model.set_workspace_filter("/src/app");
    let request = model.list_request();
    let rows = run_async(fetch_threads(&api, Some("tok"), &request)).expect("list");
    assert_eq!(rows.len(), 5, "three pages");
    let gets = hub.list_gets.lock().expect("lock");
    assert_eq!(gets.len(), 3);
    let first: Vec<_> = gets[0]
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(
        first,
        [
            ("status", "running"),
            ("agent", "claude"),
            ("workspace", "/src/app")
        ]
    );
    assert!(gets[1].contains(&("cursor".to_owned(), "2".to_owned())));
}

#[test]
fn patch_and_answer_requests_have_the_documented_shape() {
    let hub = Arc::new(Hub::default());
    let api = spawn_hub(hub.clone());
    let (mut model, _) = model_with(vec![needing("th_a", "Needs me")]);
    let rename = model.begin_rename("th_a", "Renamed").expect("rename");
    run_async(patch_thread(&api, Some("tok"), &rename)).expect("patch");
    {
        let patches = hub.patches.lock().expect("lock");
        assert_eq!(patches[0].0, "th_a");
        assert_eq!(patches[0].2, json!({ "title": "Renamed" }));
        assert_eq!(patches[0].1["authorization"], "Bearer tok");
    }
    let allow = model.begin_answer("th_a", Verdict::Allow).expect("allow");
    run_async(answer_interrupt(&api, Some("tok"), &allow)).expect("answer");
    let answers = hub.answers.lock().expect("lock");
    assert_eq!(answers[0].0, "th_a");
    assert_eq!(answers[0].1, "int_9");
    assert_eq!(
        answers[0].2["idempotency-key"],
        allow.idempotency_key.as_str()
    );
    assert_eq!(
        answers[0].3,
        json!({ "outcome": { "type": "selected", "option_id": "yes_once" } })
    );
    drop(answers);
    // An error answer is a failure with the status, and the hub's message.
    *hub.answer_status.lock().expect("lock") = 409;
    let failed = run_async(answer_interrupt(&api, Some("tok"), &allow)).expect_err("409");
    assert_eq!(failed.status, Some(409));
    assert!(failed.message.contains("already answered"), "{failed:?}");
}

// ---- strings -------------------------------------------------------------------------------------------

#[test]
fn every_string_id_the_inbox_uses_is_in_the_catalog() {
    let catalog = Catalog::from_source("en", client_gpui::inbox::EN_FTL).expect("parses");
    let mut ids = HashSet::new();
    for file in ["mod.rs", "view.rs"] {
        let source = std::fs::read_to_string(
            PathBuf::from(env!("CARGO_MANIFEST_DIR"))
                .join("src/inbox")
                .join(file),
        )
        .expect("read source");
        for line in source.lines() {
            if line.contains(".id(")
                || line.contains("Button::new(")
                || line.contains("SharedString::from")
                || line.contains("const ")
                || line.contains("\"inbox-virtual-list\"")
                || line.trim_start().starts_with("//")
            {
                continue;
            }
            let mut rest = line;
            while let Some(start) = rest.find("\"inbox-") {
                rest = &rest[start + 1..];
                if let Some(end) = rest.find('"') {
                    ids.insert(rest[..end].to_owned());
                    rest = &rest[end..];
                }
            }
        }
    }
    for slug in ["all", "needs_you", "running", "done"] {
        ids.insert(format!("inbox-filter-status-{slug}"));
    }
    for section in Section::ALL {
        ids.insert(format!("inbox-section-{}", section.slug()));
    }
    ids.retain(|id| !id.ends_with('-') && !id.contains('{') && id != "inbox-");
    assert!(ids.len() > 25, "scan found only {} ids", ids.len());
    let missing: Vec<_> = ids.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from inbox.ftl: {missing:?}"
    );
}

// ---- the view, headless ---------------------------------------------------------------------------------

fn open_inbox(
    cx: &mut TestAppContext,
    clock: Arc<ManualClock>,
) -> (AnyWindowHandle, Entity<InboxView>) {
    cx.update(gpui_kit::init);
    cx.update(|cx| {
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
            move |window, cx| cx.new(|cx| InboxView::with_clock(clock, window, cx)),
        )
        .expect("open window")
    })
}

fn wait_until(
    cx: &mut TestAppContext,
    window: AnyWindowHandle,
    view: &Entity<InboxView>,
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

fn point_at(cx: &mut TestAppContext, window: AnyWindowHandle, view: &Entity<InboxView>, api: &str) {
    let api = api.to_owned();
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |view, cx| {
            view.set_target(
                Some(Target {
                    api,
                    token: Some("tok".to_owned()),
                }),
                window,
                cx,
            );
        });
    })
    .expect("window");
}

fn sample_hub() -> (Arc<Hub>, String) {
    let hub = Arc::new(Hub::default());
    *hub.threads.lock().expect("lock") = vec![
        needing("th_need", "Needs me"),
        thread("th_run", "Fix login", "running"),
        thread("th_done", "Upgrade axum", "done"),
    ];
    let api = spawn_hub(hub.clone());
    (hub, api)
}

// ---- a row opens its thread -----------------------------------------------------------------------

/// Two rows, both with a workspace; `th_need` also has an open approval (Allow / Deny).
fn sample_hub_with_workspaces() -> (Arc<Hub>, String) {
    let hub = Arc::new(Hub::default());
    let mut need = needing("th_need", "Needs me");
    need["workspace"] = json!("/work/repo");
    let mut run = thread("th_run", "Fix login", "running");
    run["workspace"] = json!("/work/other");
    *hub.threads.lock().expect("lock") = vec![need, run];
    let api = spawn_hub(hub.clone());
    (hub, api)
}

/// Every event the inbox emits from now on, in order.
fn record_events(
    cx: &mut TestAppContext,
    view: &Entity<InboxView>,
) -> (Arc<Mutex<Vec<InboxEvent>>>, Subscription) {
    let events = Arc::new(Mutex::new(Vec::new()));
    let sink = events.clone();
    let subscription = cx.update(|cx| {
        cx.subscribe(view, move |_, event: &InboxEvent, _| {
            sink.lock().expect("lock").push(event.clone());
        })
    });
    (events, subscription)
}

#[gpui_kit::test]
fn a_row_click_opens_that_rows_thread_with_its_workspace(cx: &mut TestAppContext) {
    let (_hub, api) = sample_hub_with_workspaces();
    let (window, view) = open_inbox(cx, ManualClock::new());
    let (events, _subscription) = record_events(cx, &view);
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-open-th_run").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-open-th_run", cx);
    })
    .expect("window");
    assert_eq!(
        *events.lock().expect("lock"),
        [InboxEvent::OpenThread {
            thread_id: "th_run".to_owned(),
            workspace: Some("/work/other".to_owned()),
        }]
    );
}

#[gpui_kit::test]
fn quick_allow_answers_and_does_not_open_the_thread(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub_with_workspaces();
    let (window, view) = open_inbox(cx, ManualClock::new());
    let (events, _subscription) = record_events(cx, &view);
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-allow-th_need").is_some()
    });
    // The title click opens the thread; the Allow click after it must add no open.
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-open-th_need", cx);
    })
    .expect("window");
    let opened = events.lock().expect("lock").len();
    assert_eq!(opened, 1, "the title click opens the thread");

    cx.update_window(window, |_, window, cx| {
        window.click("inbox-allow-th_need", cx);
    })
    .expect("window");
    wait_until(cx, window, &view, |_, _| {
        !hub.answers.lock().expect("lock").is_empty()
    });
    assert_eq!(
        events.lock().expect("lock").len(),
        opened,
        "Allow must not emit an open"
    );
}

#[gpui_kit::test]
fn a_row_whose_first_entry_is_a_form_still_offers_quick_allow(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let mut row = needing("th_mix", "Mixed");
    let approval = row["pending_interrupts"][0].clone();
    row["pending_interrupts"] = json!([form_entry("int_f", "Fill in the deploy form"), approval]);
    *hub.threads.lock().expect("lock") = vec![row];
    let api = spawn_hub(hub.clone());
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-allow-th_mix").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("inbox-allow-th_mix").label(),
            Some("Allow: Allow edit to src/main.rs?")
        );
        window.click("inbox-allow-th_mix", cx);
    })
    .expect("window");
    wait_until(cx, window, &view, |_, _| {
        !hub.answers.lock().expect("lock").is_empty()
    });
    let answers = hub.answers.lock().expect("lock");
    assert_eq!(answers[0].0, "th_mix");
    assert_eq!(answers[0].1, "int_9", "the approval, not the form");
    assert_eq!(
        answers[0].3,
        json!({ "outcome": { "type": "selected", "option_id": "yes_once" } })
    );
}

#[gpui_kit::test]
fn a_row_with_only_a_form_or_auth_shows_no_quick_buttons(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    let mut form = thread("th_form", "Form", "needs_you");
    form["pending_interrupts"] = json!([form_entry("int_f", "Fill in")]);
    let mut auth = thread("th_auth", "Auth", "needs_you");
    auth["pending_interrupts"] = json!([auth_entry("int_a", "Sign in")]);
    *hub.threads.lock().expect("lock") = vec![form, auth];
    let api = spawn_hub(hub.clone());
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-open-th_form").is_some()
            && window.try_find("inbox-open-th_auth").is_some()
    });
    cx.update_window(window, |_, window, _| {
        for id in ["th_form", "th_auth"] {
            assert!(
                window.try_find(format!("inbox-allow-{id}")).is_none(),
                "{id} allow"
            );
            assert!(
                window.try_find(format!("inbox-deny-{id}")).is_none(),
                "{id} deny"
            );
        }
    })
    .expect("window");
    assert!(hub.answers.lock().expect("lock").is_empty());
}

#[gpui_kit::test]
fn the_view_shows_the_three_sections(cx: &mut TestAppContext) {
    let (_hub, api) = sample_hub();
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-done").is_some()
    });
    cx.update_window(window, |_, window, _| {
        assert_eq!(
            window.find("inbox-section-needs-you").label(),
            Some("Needs you, 1 tasks")
        );
        assert!(window.try_find("inbox-section-running").is_some());
        let rows = window.find_all("inbox-row");
        let labels: Vec<_> = rows.iter().filter_map(|r| r.label()).collect();
        assert_eq!(
            labels,
            [
                "Needs me, claude, Needs you",
                "Fix login, claude, Running",
                "Upgrade axum, claude, Done"
            ]
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn quick_allow_and_deny_answer_with_the_interrupts_own_option_and_a_key(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub();
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-allow-th_need").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            window.find("inbox-allow-th_need").label(),
            Some("Allow: Allow edit to src/main.rs?")
        );
        assert!(window.try_find("inbox-allow-th_run").is_none());
        window.click("inbox-allow-th_need", cx);
    })
    .expect("window");
    wait_until(cx, window, &view, |_, _| {
        !hub.answers.lock().expect("lock").is_empty()
    });
    {
        let answers = hub.answers.lock().expect("lock");
        assert_eq!(answers[0].0, "th_need");
        assert_eq!(answers[0].1, "int_9");
        assert!(
            answers[0].2["idempotency-key"]
                .to_str()
                .expect("key")
                .starts_with("idk_")
        );
        assert_eq!(
            answers[0].3,
            json!({ "outcome": { "type": "selected", "option_id": "yes_once" } })
        );
    }
    // The hub still lists the approval, so after the refresh Deny is offered again.
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-deny-th_need").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-deny-th_need", cx)
    })
    .expect("window");
    wait_until(cx, window, &view, |_, _| {
        hub.answers.lock().expect("lock").len() == 2
    });
    let answers = hub.answers.lock().expect("lock");
    assert_eq!(
        answers[1].3,
        json!({ "outcome": { "type": "selected", "option_id": "nope" } })
    );
    assert_ne!(
        answers[0].2["idempotency-key"],
        answers[1].2["idempotency-key"]
    );
}

#[gpui_kit::test]
fn rename_sends_the_title_and_a_refused_rename_is_rolled_back(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub();
    *hub.patch_status.lock().expect("lock") = 500;
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-rename-th_run").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-rename-th_run", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).renaming(), Some("th_run"));
        let input = view.read(cx).rename_input().clone();
        input.update(cx, |state, cx| {
            state.set_value("Fix the login flow", window, cx)
        });
        window.render_frame(cx);
        window.click("inbox-rename-save", cx);
    })
    .expect("window");
    wait_until(cx, window, &view, |_, cx| {
        view.read(cx).model().error().is_some()
    });
    {
        let patches = hub.patches.lock().expect("lock");
        assert_eq!(patches[0].0, "th_run");
        assert_eq!(patches[0].2, json!({ "title": "Fix the login flow" }));
    }
    cx.update_window(window, |_, window, cx| {
        assert_eq!(
            view.read(cx).model().row("th_run").expect("row").title,
            "Fix login"
        );
        let status = window.find("inbox-status").label().map(str::to_owned);
        assert!(
            status
                .as_deref()
                .is_some_and(|s| s.contains("the hub said no")),
            "{status:?}"
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn archive_sends_the_patch_and_the_row_leaves(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub();
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-archive-th_done").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-archive-th_done", cx);
        window.render_frame(cx);
        // Optimistic: gone before the hub has answered.
        assert!(view.read(cx).model().row("th_done").is_none());
    })
    .expect("window");
    wait_until(cx, window, &view, |_, _| {
        !hub.patches.lock().expect("lock").is_empty()
    });
    let patches = hub.patches.lock().expect("lock");
    assert_eq!(patches[0].0, "th_done");
    assert_eq!(patches[0].2, json!({ "archived": true }));
}

#[gpui_kit::test]
fn typing_in_the_search_box_fetches_after_the_debounce_not_before(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub();
    let clock = ManualClock::new();
    let (window, view) = open_inbox(cx, clock.clone());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-done").is_some()
    });
    assert_eq!(hub.list_gets.lock().expect("lock").len(), 1);
    cx.update_window(window, |_, window, cx| {
        let search = view.read(cx).search_input().focus_handle(cx);
        window.focus(&search, cx);
        window.input("axum", cx);
        window.render_frame(cx);
    })
    .expect("window");
    // Real time passes, the injected clock does not: nothing is fetched.
    for _ in 0..6 {
        cx.update_window(window, |_, window, cx| {
            view.update(cx, |view, cx| view.pump(window, cx));
        })
        .expect("window");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(hub.list_gets.lock().expect("lock").len(), 1);
    clock.advance(400);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-running").is_none()
            && window.try_find("inbox-section-done").is_some()
    });
    let gets = hub.list_gets.lock().expect("lock");
    assert_eq!(gets.len(), 2);
    assert!(
        gets[1].contains(&("q".to_owned(), "axum".to_owned())),
        "{gets:?}"
    );
}

#[gpui_kit::test]
fn status_and_agent_filters_refetch_with_their_query(cx: &mut TestAppContext) {
    let (hub, api) = sample_hub();
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-done").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-status-running", cx)
    })
    .expect("window");
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-done").is_none()
            && window.try_find("inbox-section-running").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-agent-codex", cx)
    })
    .expect("window");
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-running").is_none()
    });
    let gets = hub.list_gets.lock().expect("lock");
    let last = gets.last().expect("a request");
    assert!(last.contains(&("status".to_owned(), "running".to_owned())));
    assert!(last.contains(&("agent".to_owned(), "codex".to_owned())));
}

#[gpui_kit::test]
fn a_long_list_is_virtualized(cx: &mut TestAppContext) {
    let hub = Arc::new(Hub::default());
    *hub.threads.lock().expect("lock") = (0..400)
        .map(|i| thread(&format!("th_{i:03}"), &format!("Task {i:03}"), "done"))
        .collect();
    let api = spawn_hub(hub);
    let (window, view) = open_inbox(cx, ManualClock::new());
    point_at(cx, window, &view, &api);
    wait_until(cx, window, &view, |window, _| {
        window.try_find("inbox-section-done").is_some()
    });
    cx.update_window(window, |_, window, cx| {
        let laid_out = window.find_all("inbox-row");
        assert!(
            !laid_out.is_empty() && laid_out.len() < 40,
            "{} of 400 rows laid out",
            laid_out.len()
        );
        assert!(
            laid_out
                .iter()
                .all(|r| r.label() != Some("Task 399, claude, Done"))
        );
        window.scroll(
            "inbox-list",
            ScrollDelta::Pixels(point(px(0.), px(-48. * 200.))),
            cx,
        );
        window.render_frame(cx);
        let laid_out = window.find_all("inbox-row");
        assert!(!laid_out.is_empty() && laid_out.len() < 40);
        assert!(
            laid_out
                .iter()
                .all(|r| r.label() != Some("Task 000, claude, Done")),
            "scrolled away from the top"
        );
    })
    .expect("window");
}
