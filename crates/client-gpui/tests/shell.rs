//! M3-002: desktop shell. Sign-in by device-code pairing (QR) and GitHub OAuth PKCE against a
//! local fake hub, navigation, settings and theming, Fluent strings, accessibility labels and
//! focus order. State machines are tested directly; the view is tested on GPUI's headless platform.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::ws::{Message as WsMessage, WebSocketUpgrade};
use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use client_core::outbox::{
    FixedRandom, ManualClock, PostRequest, PostResponse, Transport, TransportError,
};
use client_core::{
    Applied, Cache, Connection, Connectivity, Intent, Outbox, RetryPolicy, Subscription, ThreadSync,
};
use client_gpui::diff::view::{OPEN_ID as DIFF_OPEN_ID, PANE_ID as DIFF_PANE_ID};
use client_gpui::shell::auth::{LoginFailure, SessionGrant, api_url};
use client_gpui::shell::browser::{Browser, RecordingBrowser};
use client_gpui::shell::i18n::{self, Catalog};
use client_gpui::shell::login::{LoginEvent, LoginMethod, LoginModel, LoginStage};
use client_gpui::shell::model::{HubOutcome, Screen, ShellModel, parse_hub};
use client_gpui::shell::nav::{NavModel, Section};
use client_gpui::shell::pairing::{self, DeviceCode, PollOutcome, PollSchedule, QrMatrix};
use client_gpui::shell::pkce::{self, Callback, GithubSignIn, LoopbackListener, Pkce};
use client_gpui::shell::prefs::{Appearance, Preferences, ThemeChoice};
use client_gpui::shell::session::{FileStore, MemoryStore, Session, SessionStore};
use client_gpui::shell::{AppConfig, AppShell};
use client_gpui::task::plan::{PLAN_EMPTY_ID, PLAN_ROW_ID, PLAN_SUMMARY_ID};
use client_gpui::task::transcript::VIEW_DIFF_ID;
use client_gpui::task::usage::{
    CONTEXT_ID, TOTAL_COST_ID, TOTAL_TOKENS_ID, TURN_COST_ID, TURN_TOKENS_ID, USAGE_ID,
};
use gpui_kit::Focusable as _;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::WindowExt as _;
use gpui_kit::test::TestWindowExt as _;
use gpui_kit::{
    AppContext, Bounds, Entity, Point, TestAppContext, WindowBounds, WindowOptions, px, size,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

// ---- a fake hub ------------------------------------------------------------------------------

#[derive(Default)]
struct HubState {
    /// Device-token script: "pending", "slow_down", "denied", "expired", "grant". Last repeats.
    script: Vec<&'static str>,
    polls: AtomicUsize,
    expires_in: u64,
    /// What the browser step registered, as the real authorize endpoint would remember it.
    challenge: Mutex<Option<String>>,
    redirect_uri: Mutex<Option<String>>,
    code_requests: AtomicUsize,
    /// Flipped by a test to play the phone approving a "gate" step.
    approved: AtomicBool,
    /// The session subscription: refused (503) until a test sets this.
    socket_up: AtomicBool,
    /// The `Authorization` header of each subscribe request.
    bearers: Mutex<Vec<String>>,
    /// POSTs to the thread's cancel endpoint, answered 200.
    cancels: AtomicUsize,
    /// Event frames the subscribe endpoint replays for a thread (after `hello`, before `live`).
    frames: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// Event frames a test pushes to a subscribed thread after `live`, sent by its socket.
    pushed: Mutex<std::collections::HashMap<String, Vec<String>>>,
    /// The threads subscribed to, in order.
    subscribed: Mutex<Vec<String>>,
    /// POSTs to an interrupt endpoint: thread, interrupt, Idempotency-Key, body.
    answers: Mutex<Vec<Answer>>,
    /// The rows `GET /v1/threads` answers with (the inbox's list).
    threads: Mutex<Vec<Value>>,
    /// Composer writes: prompts, session changes, pending removals and cancels, in order.
    writes: Mutex<Vec<Write>>,
    /// The status `DELETE /v1/threads/{t}/pending/{task}` answers (0 means 200).
    delete_status: AtomicUsize,
    /// The status `POST /v1/threads/{t}/session` answers (0 means 200).
    session_status: AtomicUsize,
    /// The `machines` array `GET /v1/machines` answers with.
    machines: Mutex<Vec<Value>>,
}

#[derive(Debug, Clone)]
struct Write {
    method: String,
    path: String,
    key: Option<String>,
    body: Value,
}

#[derive(Debug, Clone)]
struct Answer {
    thread: String,
    interrupt: String,
    key: Option<String>,
    body: Value,
}

fn grant_body() -> Value {
    json!({
        "token": "sm_session_token",
        "expires_at": 1_900_000_000,
        "user": { "display_name": "Ada", "github_login": "ada" },
        "org": { "name": "Acme" },
        "role": "member"
    })
}

async fn device_code(State(hub): State<Arc<HubState>>) -> Json<Value> {
    hub.code_requests.fetch_add(1, Ordering::SeqCst);
    Json(json!({
        "device_code": "dev-code",
        "user_code": "ABCD-EFGH",
        "verification_uri": "https://hub.test/pair/device",
        "verification_uri_complete": "https://hub.test/pair/device?user_code=ABCD-EFGH",
        "expires_in": hub.expires_in,
        "interval": 1
    }))
}

async fn device_token(
    State(hub): State<Arc<HubState>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    if body["device_code"] != "dev-code" {
        return (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        );
    }
    let index = hub.polls.fetch_add(1, Ordering::SeqCst);
    let step = hub
        .script
        .get(index)
        .or(hub.script.last())
        .copied()
        .unwrap_or("pending");
    match step {
        "grant" => (StatusCode::OK, Json(grant_body())),
        "gate" if hub.approved.load(Ordering::SeqCst) => (StatusCode::OK, Json(grant_body())),
        "slow_down" => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "slow_down" })),
        ),
        "denied" => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "access_denied" })),
        ),
        "expired" => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "expired_token" })),
        ),
        _ => (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "authorization_pending" })),
        ),
    }
}

async fn github_token(
    State(hub): State<Arc<HubState>>,
    Json(body): Json<Value>,
) -> (StatusCode, Json<Value>) {
    let verifier = body["code_verifier"].as_str().unwrap_or_default();
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let registered = hub.challenge.lock().expect("lock").clone();
    let redirect = hub.redirect_uri.lock().expect("lock").clone();
    let ok = body["code"] == "hub-code"
        && registered.as_deref() == Some(challenge.as_str())
        && redirect.as_deref() == body["redirect_uri"].as_str();
    if ok {
        (StatusCode::CREATED, Json(grant_body()))
    } else {
        (
            StatusCode::BAD_REQUEST,
            Json(json!({ "error": "invalid_grant" })),
        )
    }
}

async fn session_subscribe(
    State(hub): State<Arc<HubState>>,
    Path(thread_id): Path<String>,
    headers: axum::http::HeaderMap,
    upgrade: WebSocketUpgrade,
) -> Response {
    let bearer = headers
        .get("authorization")
        .and_then(|value| value.to_str().ok())
        .unwrap_or_default()
        .to_owned();
    hub.bearers.lock().expect("lock").push(bearer);
    hub.subscribed.lock().expect("lock").push(thread_id.clone());
    if !hub.socket_up.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    let frames = hub
        .frames
        .lock()
        .expect("lock")
        .get(&thread_id)
        .cloned()
        .unwrap_or_default();
    let hub = hub.clone();
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
        // Send what a test pushes for this thread, and watch for the client closing.
        loop {
            let pushed = hub
                .pushed
                .lock()
                .expect("lock")
                .remove(&thread_id)
                .unwrap_or_default();
            for frame in pushed {
                let _ = socket.send(WsMessage::Text(frame.into())).await;
            }
            match tokio::time::timeout(Duration::from_millis(20), socket.recv()).await {
                Ok(Some(Ok(_))) => {}
                Ok(_) => break,
                Err(_) => {}
            }
        }
    })
}

/// Pushes an event frame to the subscription of `thread` (it is sent once the socket is live).
fn push_live(state: &HubState, thread: &str, frame: String) {
    state
        .pushed
        .lock()
        .expect("lock")
        .entry(thread.to_owned())
        .or_default()
        .push(frame);
}

async fn session_answer(
    State(hub): State<Arc<HubState>>,
    Path((thread, interrupt)): Path<(String, String)>,
    headers: axum::http::HeaderMap,
    Json(body): Json<Value>,
) -> Json<Value> {
    hub.answers.lock().expect("lock").push(Answer {
        thread,
        interrupt,
        key: headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body,
    });
    Json(json!({}))
}

async fn record_write(
    State(hub): State<Arc<HubState>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    hub.writes.lock().expect("lock").push(Write {
        method: method.to_string(),
        path: uri.path().to_owned(),
        key: headers
            .get("idempotency-key")
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned),
        body: serde_json::from_slice(&body).unwrap_or(Value::Null),
    });
    let status = if method == axum::http::Method::DELETE {
        hub.delete_status.load(Ordering::SeqCst)
    } else if uri.path().ends_with("/session") {
        hub.session_status.load(Ordering::SeqCst)
    } else {
        0
    };
    if status != 0 {
        let code = StatusCode::from_u16(status as u16).expect("status");
        let error = match status {
            409 => "task_started",
            403 => "forbidden",
            418 => "teapot",
            _ => "invalid_request",
        };
        return (code, Json(json!({ "error": error }))).into_response();
    }
    Json(json!({})).into_response()
}

async fn record_cancel(
    State(hub): State<Arc<HubState>>,
    method: axum::http::Method,
    uri: axum::http::Uri,
    headers: axum::http::HeaderMap,
    body: axum::body::Bytes,
) -> Response {
    hub.cancels.fetch_add(1, Ordering::SeqCst);
    record_write(State(hub), method, uri, headers, body).await
}

async fn list_machines(State(state): State<Arc<HubState>>) -> Json<Value> {
    Json(json!({ "machines": state.machines.lock().expect("lock").clone() }))
}

/// The inbox's list: the rows the test put in `threads`, on one page.
async fn list_threads(State(state): State<Arc<HubState>>) -> Json<Value> {
    let threads = state.threads.lock().expect("lock").clone();
    Json(json!({ "threads": threads, "next_cursor": null }))
}

fn spawn_hub(state: Arc<HubState>) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("hub runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/v1/auth/device/code", post(device_code))
                .route("/v1/auth/device/token", post(device_token))
                .route("/v1/auth/github/token", post(github_token))
                .route("/v1/threads", get(list_threads))
                .route("/v1/threads/{thread_id}/subscribe", get(session_subscribe))
                .route("/v1/machines", get(list_machines))
                .route("/v1/threads/{thread_id}/prompts", post(record_write))
                .route("/v1/threads/{thread_id}/session", post(record_write))
                .route(
                    "/v1/threads/{thread_id}/pending/{task_id}",
                    axum::routing::delete(record_write),
                )
                .route("/v1/threads/{thread_id}/cancel", post(record_cancel))
                .route(
                    "/v1/threads/{thread_id}/interrupts/{interrupt_id}",
                    post(session_answer),
                )
                .with_state(state);
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

fn hub_with(script: Vec<&'static str>, expires_in: u64) -> (Arc<HubState>, String) {
    let state = Arc::new(HubState {
        script,
        expires_in,
        ..HubState::default()
    });
    let url = spawn_hub(state.clone());
    (state, url)
}

/// A fake hub for the subscription: while `up` is false the upgrade is refused (503), as an
/// unreachable or failing hub would answer. When it is up, the socket sends `live` and stays open.
async fn subscribe(State(up): State<Arc<AtomicBool>>, upgrade: WebSocketUpgrade) -> Response {
    if !up.load(Ordering::SeqCst) {
        return StatusCode::SERVICE_UNAVAILABLE.into_response();
    }
    upgrade.on_upgrade(|mut socket| async move {
        let _ = socket
            .send(WsMessage::Text(r#"{"type":"live"}"#.into()))
            .await;
        while let Some(Ok(_)) = socket.recv().await {}
    })
}

fn spawn_socket_hub(up: Arc<AtomicBool>) -> String {
    let (sender, receiver) = std::sync::mpsc::channel();
    std::thread::spawn(move || {
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(1)
            .enable_all()
            .build()
            .expect("socket hub runtime");
        runtime.block_on(async move {
            let app = Router::new()
                .route("/threads/{thread_id}/subscribe", get(subscribe))
                .with_state(up);
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
                .await
                .expect("bind socket hub");
            sender
                .send(listener.local_addr().expect("addr").port())
                .expect("send port");
            axum::serve(listener, app).await.expect("serve");
        });
    });
    format!(
        "http://127.0.0.1:{}",
        receiver.recv().expect("socket hub port")
    )
}

/// Answers every POST with 200, so the outbox's link comes up.
struct Answering;

impl Transport for Answering {
    fn post(
        &self,
        _request: PostRequest,
    ) -> impl std::future::Future<Output = Result<PostResponse, TransportError>> + Send {
        std::future::ready(Ok(PostResponse {
            status: 200,
            body: serde_json::json!({}),
        }))
    }
}

async fn instant(_: Duration) {}

fn unique_dir(name: &str) -> std::path::PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "client-gpui-shell-{name}-{}-{}",
        std::process::id(),
        pkce::random_token(6)
    ));
    std::fs::create_dir_all(&dir).expect("temp dir");
    dir
}

// ---- Fluent strings --------------------------------------------------------------------------

/// Message ids a source file uses: `tr("id")`, `tr_args("id", ..)` and `"…-…"` ids in tables.
fn ids_used_in_sources() -> HashSet<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src/shell");
    let mut ids = HashSet::new();
    for entry in std::fs::read_dir(dir).expect("src/shell") {
        let path = entry.expect("entry").path();
        if path.extension().is_none_or(|ext| ext != "rs") {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("read source");
        for call in ["tr(\"", "tr_args(\"", "message_id"] {
            let mut rest = text.as_str();
            while let Some(at) = rest.find(call) {
                rest = &rest[at + call.len()..];
                if call == "message_id" {
                    continue;
                }
                if let Some(end) = rest.find('"') {
                    ids.insert(rest[..end].to_string());
                }
            }
        }
        // Ids in `=> "xxx-yyy",` match arms and `"xxx-yyy" ` literals inside message_id tables.
        for line in text.lines() {
            let line = line.trim();
            if line.contains("=> \"")
                && let Some(start) = line.find("=> \"")
            {
                let rest = &line[start + 4..];
                if let Some(end) = rest.find('"') {
                    let id = &rest[..end];
                    if id.contains('-') && id.chars().all(|c| c.is_ascii_lowercase() || c == '-') {
                        ids.insert(id.to_string());
                    }
                }
            }
        }
    }
    ids
}

#[test]
fn every_string_id_used_by_the_shell_exists_in_the_english_catalog() {
    let catalog = Catalog::english();
    let used = ids_used_in_sources();
    assert!(used.len() > 30, "scan found only {} ids", used.len());
    let missing: Vec<_> = used
        .iter()
        .filter(|id| !catalog.has(id) && id.starts_with(|c: char| c.is_ascii_lowercase()))
        .filter(|id| {
            ["login-", "nav-", "settings-", "banner-", "content-"]
                .iter()
                .any(|p| id.starts_with(p))
        })
        .collect();
    assert!(
        missing.is_empty(),
        "ids missing from shell.ftl: {missing:?}"
    );
}

#[test]
fn catalog_formats_arguments_and_reports_missing_ids_visibly() {
    let catalog = Catalog::english();
    assert_eq!(
        catalog.tr("login-pairing-code", &[("code", "ABCD-EFGH")]),
        "Code: ABCD-EFGH"
    );
    assert_eq!(
        catalog.tr("login-error-network", &[("detail", "refused")]),
        "Could not reach the hub: refused"
    );
    assert_eq!(catalog.tr("no-such-message", &[]), "no-such-message");
    for id in Catalog::ids(i18n::EN_FTL) {
        let text = catalog.tr(
            &id,
            &[
                ("code", "X"),
                ("detail", "X"),
                ("user", "X"),
                ("org", "X"),
                ("section", "X"),
                ("choice", "X"),
                ("path", "X"),
                ("added", "X"),
                ("removed", "X"),
                ("uri", "X"),
                ("minutes", "5"),
                ("hub", "X"),
            ],
        );
        assert!(!text.trim().is_empty(), "{id} is empty");
        assert!(
            !text.contains('{') && text != id,
            "{id} did not format: {text}"
        );
    }
}

#[test]
fn catalog_rejects_a_broken_source() {
    assert!(Catalog::from_source("en", "broken = { $x").is_err());
    assert!(Catalog::from_source("not a locale!!", "a = b").is_err());
}

// ---- PKCE and the loopback redirect -----------------------------------------------------------

#[test]
fn pkce_challenge_matches_the_rfc_7636_vector() {
    let pkce = Pkce::from_verifier("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk".to_string());
    assert_eq!(
        pkce.challenge,
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
    let fresh = Pkce::generate();
    assert_eq!(fresh.verifier.len(), 43);
    assert_eq!(fresh.challenge.len(), 43);
    assert_ne!(fresh, Pkce::generate());
}

#[test]
fn authorize_url_carries_s256_state_and_the_loopback_redirect() {
    let pkce = Pkce::generate();
    let url = pkce::authorize_url(
        "https://hub.example/",
        "http://127.0.0.1:4455/callback",
        &pkce,
        "state-1",
        Some("org_1"),
    );
    let parsed = url::Url::parse(&url).expect("url");
    assert_eq!(parsed.path(), "/v1/auth/github/authorize");
    let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    assert_eq!(query["code_challenge_method"], "S256");
    assert_eq!(query["code_challenge"], pkce.challenge);
    assert_eq!(query["state"], "state-1");
    assert_eq!(query["redirect_uri"], "http://127.0.0.1:4455/callback");
    assert_eq!(query["org_id"], "org_1");
    assert!(
        !url.contains(&pkce.verifier),
        "the verifier never leaves the app"
    );
}

#[test]
fn callback_parsing_accepts_only_our_state_on_the_callback_path() {
    let ok = "GET /callback?code=abc&state=s1 HTTP/1.1";
    assert_eq!(pkce::parse_callback(ok, "s1"), Callback::Code("abc".into()));
    assert_eq!(pkce::parse_callback(ok, "other"), Callback::Ignored);
    assert_eq!(
        pkce::parse_callback("GET /other?code=abc&state=s1 HTTP/1.1", "s1"),
        Callback::Ignored
    );
    assert_eq!(
        pkce::parse_callback("GET /callback?code=abc HTTP/1.1", "s1"),
        Callback::Ignored
    );
    assert_eq!(
        pkce::parse_callback("GET /callback?error=access_denied&state=s1 HTTP/1.1", "s1"),
        Callback::Error("access_denied".into())
    );
    assert_eq!(
        pkce::parse_callback("POST /callback?code=abc&state=s1 HTTP/1.1", "s1"),
        Callback::Ignored
    );
}

async fn hit(url: &str) -> u16 {
    reqwest::get(url).await.expect("request").status().as_u16()
}

/// Play the browser: read what was opened, register it with the fake hub the way the real
/// authorize endpoint would, then follow the redirect to the loopback port.
async fn browser_follows(
    hub: &HubState,
    browser: &RecordingBrowser,
    stray_first: bool,
    outcome: &str,
) -> (String, String) {
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    let opened = loop {
        if let Some(url) = browser.opened().first() {
            break url.clone();
        }
        assert!(
            std::time::Instant::now() < deadline,
            "the sign-in never opened the browser"
        );
        tokio::time::sleep(Duration::from_millis(10)).await;
    };
    let parsed = url::Url::parse(&opened).expect("authorize url");
    let query: std::collections::HashMap<_, _> = parsed.query_pairs().into_owned().collect();
    *hub.challenge.lock().expect("lock") = Some(query["code_challenge"].clone());
    *hub.redirect_uri.lock().expect("lock") = Some(query["redirect_uri"].clone());
    let redirect = query["redirect_uri"].clone();
    let state = query["state"].clone();
    if stray_first {
        assert_eq!(hit(&format!("{redirect}?code=evil&state=wrong")).await, 400);
        let origin = redirect.trim_end_matches("/callback");
        assert_eq!(hit(&format!("{origin}/favicon.ico")).await, 400);
    }
    let back = match outcome {
        "denied" => format!("{redirect}?error=access_denied&state={state}"),
        _ => format!("{redirect}?code=hub-code&state={state}"),
    };
    assert_eq!(hit(&back).await, 200);
    (query["code_challenge"].clone(), redirect)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_sign_in_runs_pkce_through_the_browser_and_a_loopback_redirect() {
    let (hub_state, hub) = hub_with(vec![], 600);
    let browser = Arc::new(RecordingBrowser::new());
    let settings = GithubSignIn {
        hub,
        org_id: None,
        timeout: Duration::from_secs(10),
    };
    let http = reqwest::Client::new();
    let cancel = AtomicBool::new(false);
    let opened_flag = AtomicBool::new(false);
    let (result, (challenge, redirect)) = tokio::join!(
        pkce::sign_in(&settings, browser.as_ref(), &http, &cancel, || {
            opened_flag.store(true, Ordering::SeqCst)
        }),
        browser_follows(&hub_state, &browser, true, "ok"),
    );
    let grant = result.expect("signed in");
    assert_eq!(grant.token, "sm_session_token");
    assert_eq!(grant.user_label(), "Ada");
    assert_eq!(grant.org_label(), "Acme");
    assert_eq!(grant.role.as_deref(), Some("member"));
    assert!(opened_flag.load(Ordering::SeqCst));
    assert_eq!(challenge.len(), 43);
    assert!(redirect.starts_with("http://127.0.0.1:") && redirect.ends_with("/callback"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn github_sign_in_reports_a_declined_account() {
    let (hub_state, hub) = hub_with(vec![], 600);
    let browser = Arc::new(RecordingBrowser::new());
    let settings = GithubSignIn {
        hub,
        org_id: None,
        timeout: Duration::from_secs(10),
    };
    let http = reqwest::Client::new();
    let cancel = AtomicBool::new(false);
    let (result, _) = tokio::join!(
        pkce::sign_in(&settings, browser.as_ref(), &http, &cancel, || {}),
        browser_follows(&hub_state, &browser, false, "denied"),
    );
    assert_eq!(result, Err(LoginFailure::Denied));
}

#[tokio::test]
async fn github_sign_in_fails_cleanly_without_a_browser_or_a_redirect() {
    let settings = GithubSignIn {
        hub: "http://127.0.0.1:9".to_string(),
        org_id: None,
        timeout: Duration::from_millis(300),
    };
    let http = reqwest::Client::new();
    let cancel = AtomicBool::new(false);

    let broken = RecordingBrowser::failing("no display");
    let result = pkce::sign_in(&settings, &broken, &http, &cancel, || {}).await;
    assert_eq!(result, Err(LoginFailure::Browser("no display".into())));

    let silent = RecordingBrowser::new();
    let result = pkce::sign_in(&settings, &silent, &http, &cancel, || {}).await;
    assert_eq!(result, Err(LoginFailure::Timeout));

    cancel.store(true, Ordering::SeqCst);
    let result = pkce::sign_in(&settings, &silent, &http, &cancel, || {}).await;
    assert_eq!(result, Err(LoginFailure::Cancelled));
}

#[tokio::test]
async fn token_exchange_with_a_wrong_verifier_is_invalid_grant() {
    let (hub_state, hub) = hub_with(vec![], 600);
    let pkce = Pkce::generate();
    *hub_state.challenge.lock().expect("lock") = Some(pkce.challenge.clone());
    *hub_state.redirect_uri.lock().expect("lock") = Some("http://127.0.0.1:1/callback".into());
    let http = reqwest::Client::new();
    let bad = pkce::exchange(
        &http,
        &hub,
        "hub-code",
        "not-the-verifier",
        "http://127.0.0.1:1/callback",
    )
    .await;
    assert_eq!(bad, Err(LoginFailure::Hub("invalid_grant".into())));
    let good = pkce::exchange(
        &http,
        &hub,
        "hub-code",
        &pkce.verifier,
        "http://127.0.0.1:1/callback",
    )
    .await;
    assert_eq!(good.expect("exchange").token, "sm_session_token");
}

#[tokio::test]
async fn the_loopback_listener_binds_127_0_0_1_on_an_ephemeral_port() {
    let listener = LoopbackListener::bind().await.expect("bind");
    assert_ne!(listener.port(), 0);
    assert_eq!(
        listener.redirect_uri(),
        format!("http://127.0.0.1:{}/callback", listener.port())
    );
}

// ---- device-code pairing ----------------------------------------------------------------------

fn sample_code(expires_in: u64) -> DeviceCode {
    DeviceCode::from_json(&json!({
        "device_code": "d", "user_code": "ABCD-EFGH",
        "verification_uri": "https://hub.test/pair/device",
        "interval": 5, "expires_in": expires_in
    }))
    .expect("device code")
}

#[test]
fn poll_answers_are_classified_per_rfc_8628() {
    let err = |code: &str| pairing::classify_poll(400, &json!({ "error": code }));
    assert_eq!(err("authorization_pending"), PollOutcome::Pending);
    assert_eq!(err("slow_down"), PollOutcome::SlowDown(None));
    assert_eq!(
        pairing::classify_poll(400, &json!({ "error": "slow_down", "interval": 12 })),
        PollOutcome::SlowDown(Some(12))
    );
    assert_eq!(err("expired_token"), PollOutcome::Expired);
    assert_eq!(err("access_denied"), PollOutcome::Denied);
    assert_eq!(
        err("weird"),
        PollOutcome::Failed(LoginFailure::Hub("weird".into()))
    );
    assert!(matches!(
        pairing::classify_poll(200, &grant_body()),
        PollOutcome::Granted(_)
    ));
    assert!(matches!(
        pairing::classify_poll(200, &json!({})),
        PollOutcome::Failed(_)
    ));
}

#[test]
fn the_poll_schedule_slows_down_and_stops_at_expiry() {
    let mut schedule = PollSchedule::new(&sample_code(12));
    assert_eq!(schedule.interval(), Duration::from_secs(5));
    schedule.slow_down(None);
    assert_eq!(schedule.interval(), Duration::from_secs(10));
    schedule.slow_down(Some(30));
    assert_eq!(schedule.interval(), Duration::from_secs(30));
    schedule.slow_down(Some(1));
    assert_eq!(schedule.interval(), Duration::from_secs(35));
    let mut short = PollSchedule::new(&sample_code(10));
    assert!(short.wait_done());
    assert!(short.wait_done());
    assert!(!short.wait_done());
}

#[test]
fn the_qr_target_prefers_the_complete_link() {
    let mut code = sample_code(600);
    assert_eq!(code.qr_target(), "https://hub.test/pair/device");
    code.verification_uri_complete =
        Some("https://hub.test/pair/device?user_code=ABCD-EFGH".into());
    assert!(code.qr_target().ends_with("user_code=ABCD-EFGH"));
}

#[test]
fn the_qr_matrix_has_finder_patterns_and_run_length_rows() {
    let qr = QrMatrix::encode("https://hub.test/pair/device?user_code=ABCD-EFGH").expect("qr");
    assert!(qr.width() >= 21 && (qr.width() - 21).is_multiple_of(4));
    let last = qr.width() - 1;
    for (x, y) in [(0, 0), (6, 0), (0, 6), (6, 6), (last, 0), (0, last)] {
        assert!(qr.is_dark(x, y), "finder corner ({x},{y})");
    }
    assert!(!qr.is_dark(7, 0), "separator next to the finder pattern");
    assert_eq!(qr.dark_runs(0).first(), Some(&(0, 7)));
    assert!(!qr.is_dark(qr.width(), 0), "out of range is light");
}

#[tokio::test]
async fn pairing_polls_with_backoff_until_the_phone_approves() {
    let (state, hub) = hub_with(vec!["pending", "slow_down", "pending", "grant"], 600);
    let slept = Mutex::new(Vec::new());
    let shown = Mutex::new(None);
    let cancel = AtomicBool::new(false);
    let http = reqwest::Client::new();
    let grant = pairing::run(
        &http,
        &hub,
        &cancel,
        |duration| {
            slept.lock().expect("lock").push(duration);
            instant(duration)
        },
        |code| *shown.lock().expect("lock") = Some(code.clone()),
    )
    .await
    .expect("granted");
    assert_eq!(grant.token, "sm_session_token");
    assert_eq!(state.polls.load(Ordering::SeqCst), 4);
    let secs: Vec<u64> = slept
        .lock()
        .expect("lock")
        .iter()
        .map(Duration::as_secs)
        .collect();
    assert_eq!(
        secs,
        vec![1, 1, 6, 6],
        "interval grows by 5 s after slow_down"
    );
    let shown = shown.lock().expect("lock").clone().expect("code shown");
    assert_eq!(shown.user_code, "ABCD-EFGH");
}

#[tokio::test]
async fn pairing_ends_on_denial_expiry_and_cancel() {
    let http = reqwest::Client::new();
    let cancel = AtomicBool::new(false);

    let (_, hub) = hub_with(vec!["pending", "denied"], 600);
    let result = pairing::run(&http, &hub, &cancel, instant, |_| {}).await;
    assert_eq!(result, Err(LoginFailure::Denied));

    let (_, hub) = hub_with(vec!["expired"], 600);
    let result = pairing::run(&http, &hub, &cancel, instant, |_| {}).await;
    assert_eq!(result, Err(LoginFailure::Expired));

    // The hub keeps saying pending; the client stops at the code's own lifetime.
    let (state, hub) = hub_with(vec!["pending"], 3);
    let result = pairing::run(&http, &hub, &cancel, instant, |_| {}).await;
    assert_eq!(result, Err(LoginFailure::Expired));
    assert_eq!(state.polls.load(Ordering::SeqCst), 3);

    let (state, hub) = hub_with(vec!["pending"], 600);
    cancel.store(true, Ordering::SeqCst);
    let result = pairing::run(&http, &hub, &cancel, instant, |_| {}).await;
    assert_eq!(result, Err(LoginFailure::Cancelled));
    assert_eq!(
        state.polls.load(Ordering::SeqCst),
        0,
        "no poll after cancel"
    );
}

#[tokio::test]
async fn an_unreachable_hub_is_a_network_failure() {
    let http = reqwest::Client::new();
    let cancel = AtomicBool::new(false);
    let result = pairing::run(&http, "http://127.0.0.1:9", &cancel, instant, |_| {}).await;
    assert!(
        matches!(result, Err(LoginFailure::Network(_))),
        "{result:?}"
    );
}

// ---- state machines ----------------------------------------------------------------------------

fn grant() -> SessionGrant {
    SessionGrant::from_json(&grant_body()).expect("grant")
}

#[test]
fn the_login_model_walks_pairing_and_github_and_recovers_from_failure() {
    let mut login = LoginModel::new();
    assert_eq!(login.stage(), &LoginStage::Choose);
    assert!(!login.is_busy());

    login.apply(LoginEvent::Started(LoginMethod::Pairing));
    assert!(login.is_busy());
    login.apply(LoginEvent::PairingCode(sample_code(600)));
    match login.stage() {
        LoginStage::Pairing {
            user_code,
            expires_minutes,
            qr_target,
            ..
        } => {
            assert_eq!(user_code, "ABCD-EFGH");
            assert_eq!(*expires_minutes, 10);
            assert_eq!(qr_target, "https://hub.test/pair/device");
        }
        other => panic!("{other:?}"),
    }
    assert!(login.status_text().contains("https://hub.test/pair/device"));
    login.apply(LoginEvent::Finished(Err(LoginFailure::Expired)));
    assert_eq!(
        login.status_text(),
        "The code expired before it was approved."
    );
    login.apply(LoginEvent::Started(LoginMethod::GitHub));
    login.apply(LoginEvent::BrowserOpened);
    assert_eq!(login.stage(), &LoginStage::Browser);
    login.apply(LoginEvent::Reset);
    assert_eq!(login.stage(), &LoginStage::Choose);

    // The cancelled flow reporting back must not disturb a new one.
    login.apply(LoginEvent::Started(LoginMethod::Pairing));
    login.apply(LoginEvent::Finished(Err(LoginFailure::Cancelled)));
    assert_eq!(login.stage(), &LoginStage::Starting(LoginMethod::Pairing));
    // A pairing code cannot arrive in a GitHub flow.
    let mut github = LoginModel::new();
    github.apply(LoginEvent::Started(LoginMethod::GitHub));
    github.apply(LoginEvent::PairingCode(sample_code(60)));
    assert_eq!(github.stage(), &LoginStage::Starting(LoginMethod::GitHub));

    login.apply(LoginEvent::Finished(Ok(grant())));
    assert!(matches!(login.stage(), LoginStage::SignedIn(_)));
    login.apply(LoginEvent::Reset);
    assert!(matches!(login.stage(), LoginStage::SignedIn(_)), "final");
    assert_eq!(login.status_text(), "Signed in as Ada");
}

fn model(store: Arc<dyn SessionStore>, prefs_path: Option<std::path::PathBuf>) -> ShellModel {
    ShellModel::new(
        store,
        Preferences::default(),
        prefs_path,
        "https://hub.test",
    )
}

#[test]
fn signing_in_stores_the_session_and_signing_out_forgets_it() {
    let store = Arc::new(MemoryStore::default());
    let mut shell = model(store.clone(), None);
    assert_eq!(shell.screen(), Screen::Login);
    shell
        .apply_login(LoginEvent::Started(LoginMethod::Pairing))
        .expect("start");
    assert_eq!(shell.screen(), Screen::Login);
    shell
        .apply_login(LoginEvent::Finished(Ok(grant())))
        .expect("finish");
    assert_eq!(shell.screen(), Screen::Main);
    let saved = store.load().expect("session saved");
    assert_eq!(saved.hub, "https://hub.test");
    assert_eq!(saved.grant.token, "sm_session_token");

    // The next launch starts signed in.
    let relaunch = model(store.clone(), None);
    assert_eq!(relaunch.screen(), Screen::Main);

    shell.sign_out().expect("sign out");
    assert_eq!(shell.screen(), Screen::Login);
    assert_eq!(shell.login.stage(), &LoginStage::Choose);
    assert!(store.load().is_none());
}

#[test]
fn the_file_store_round_trips_and_keeps_the_token_private() {
    let dir = unique_dir("session");
    let store = FileStore::new(dir.join("nested/session.json"));
    assert!(store.load().is_none());
    let session = Session {
        hub: "https://hub.test".into(),
        grant: grant(),
    };
    store.save(&session).expect("save");
    assert_eq!(store.load(), Some(session));
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt as _;
        let mode = std::fs::metadata(dir.join("nested/session.json"))
            .expect("meta")
            .permissions()
            .mode();
        assert_eq!(
            mode & 0o077,
            0,
            "session file must not be group/world readable"
        );
    }
    store.clear().expect("clear");
    store.clear().expect("clearing twice is fine");
    assert!(store.load().is_none());
}

#[test]
fn theme_choice_resolves_and_persists() {
    assert_eq!(ThemeChoice::System.resolve(true), Appearance::Dark);
    assert_eq!(ThemeChoice::System.resolve(false), Appearance::Light);
    assert_eq!(ThemeChoice::Light.resolve(true), Appearance::Light);
    assert_eq!(ThemeChoice::Dark.resolve(false), Appearance::Dark);

    let dir = unique_dir("prefs");
    let path = dir.join("preferences.json");
    let mut shell = model(Arc::new(MemoryStore::default()), Some(path.clone()));
    assert_eq!(shell.appearance(true), Appearance::Dark);
    shell.set_theme(ThemeChoice::Light).expect("set theme");
    shell.set_hub("https://other.test").expect("set hub");
    assert_eq!(shell.appearance(true), Appearance::Light);
    let loaded = Preferences::load(&path);
    assert_eq!(loaded.theme, ThemeChoice::Light);
    assert_eq!(loaded.hub_url, "https://other.test");
    assert_eq!(
        Preferences::load(&dir.join("missing.json")),
        Preferences::default()
    );
}

#[test]
fn navigation_wraps_and_labels_the_current_page() {
    let mut nav = NavModel::default();
    assert_eq!(nav.current(), Section::Inbox);
    nav.previous();
    assert_eq!(nav.current(), Section::Settings);
    nav.next();
    assert_eq!(nav.current(), Section::Inbox);
    nav.select(Section::Machines);
    assert_eq!(
        Section::Machines.a11y_label(Section::Machines),
        "Machines, current page"
    );
    assert_eq!(Section::Inbox.a11y_label(Section::Machines), "Go to Inbox");
    let slugs: HashSet<_> = Section::ALL.iter().map(|s| s.slug()).collect();
    assert_eq!(slugs.len(), Section::ALL.len());
}

#[test]
fn the_banner_follows_connectivity() {
    let mut shell = model(Arc::new(MemoryStore::default()), None);
    shell.set_connectivity(Connectivity::Online);
    assert_eq!(shell.banner(), None);
    shell.set_connectivity(Connectivity::Offline);
    assert_eq!(
        shell.banner().as_deref(),
        Some("You are offline. Changes will be sent when the connection returns.")
    );
    shell.set_connectivity(Connectivity::Connecting);
    assert_eq!(shell.banner().as_deref(), Some("Reconnecting..."));
}

#[test]
fn focus_order_is_stable_labelled_and_unique_on_every_screen() {
    let store = Arc::new(MemoryStore::default());
    let mut shell = model(store, None);
    let check = |shell: &ShellModel, expected: &[&str]| {
        let slots = shell.focus_order();
        let ids: Vec<&str> = slots.iter().map(|s| s.id.as_str()).collect();
        assert_eq!(ids, expected);
        let unique: HashSet<_> = ids.iter().collect();
        assert_eq!(unique.len(), ids.len());
        assert!(
            slots
                .iter()
                .all(|s| !s.label.trim().is_empty() && !s.label.contains("-a11y"))
        );
    };
    check(&shell, &["login-pair", "login-github"]);
    shell
        .apply_login(LoginEvent::Started(LoginMethod::GitHub))
        .expect("start");
    check(&shell, &["login-cancel"]);
    shell
        .apply_login(LoginEvent::Finished(Err(LoginFailure::Timeout)))
        .expect("fail");
    check(&shell, &["login-retry"]);
    shell.apply_login(LoginEvent::Reset).expect("reset");
    shell
        .apply_login(LoginEvent::Started(LoginMethod::GitHub))
        .expect("start");
    shell
        .apply_login(LoginEvent::Finished(Ok(grant())))
        .expect("done");
    let nav: Vec<String> = Section::ALL.iter().map(|s| s.nav_id()).collect();
    let nav_refs: Vec<&str> = nav.iter().map(String::as_str).collect();
    check(&shell, &nav_refs);
    shell.nav.select(Section::Settings);
    let mut with_settings = nav_refs.clone();
    with_settings.extend([
        "theme-system",
        "theme-light",
        "theme-dark",
        "hub-address",
        "hub-save",
        "sign-out",
    ]);
    check(&shell, &with_settings);
}

// ---- the view, headless -------------------------------------------------------------------------

fn open_shell(
    cx: &mut TestAppContext,
    hub: &str,
    store: Arc<dyn SessionStore>,
    browser: Arc<dyn Browser>,
) -> (gpui_kit::AnyWindowHandle, Entity<AppShell>) {
    open_shell_at(cx, hub, store, browser, None)
}

fn open_shell_at(
    cx: &mut TestAppContext,
    hub: &str,
    store: Arc<dyn SessionStore>,
    browser: Arc<dyn Browser>,
    prefs_path: Option<std::path::PathBuf>,
) -> (gpui_kit::AnyWindowHandle, Entity<AppShell>) {
    open_shell_with(
        cx,
        hub,
        store,
        browser,
        prefs_path,
        "th_1",
        Arc::new(client_core::outbox::SystemClock),
    )
}

fn open_shell_with(
    cx: &mut TestAppContext,
    hub: &str,
    store: Arc<dyn SessionStore>,
    browser: Arc<dyn Browser>,
    prefs_path: Option<std::path::PathBuf>,
    link_thread: &str,
    clock: Arc<dyn client_core::outbox::Clock>,
) -> (gpui_kit::AnyWindowHandle, Entity<AppShell>) {
    let link_thread = link_thread.to_owned();
    cx.update(|cx| {
        gpui_kit::init(cx);
        client_gpui::shell::init(cx);
    });
    let hub = hub.to_string();
    cx.update(|cx| {
        let bounds = Bounds {
            origin: Point::default(),
            size: size(px(1000.), px(700.)),
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
                            hub,
                            browser,
                            store,
                            prefs: Preferences::default(),
                            prefs_path,
                            poll_speedup: 200,
                            link_thread,
                            clock,
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

/// Pump the background sign-in until `done` holds (real time; the fake hub is local).
fn wait_until(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
    done: impl Fn(&AppShell) -> bool,
) {
    for _ in 0..400 {
        let finished = cx
            .update_window(window, |_, window, cx| {
                view.update(cx, |shell, cx| shell.pump_login(cx));
                window.render_frame(cx);
                done(view.read(cx))
            })
            .expect("window");
        if finished {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for the sign-in");
}

#[gpui_kit::test]
fn the_login_screen_offers_both_methods_with_labels_in_focus_order(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        "http://127.0.0.1:9",
        Arc::new(MemoryStore::default()),
        Arc::new(RecordingBrowser::new()),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let slots = view.read(cx).model().focus_order();
        assert_eq!(slots.len(), 2);
        for slot in &slots {
            let found = window.find(slot.id.clone());
            assert_eq!(found.label(), Some(slot.label.as_str()), "{}", slot.id);
            assert!(found.visible());
        }
        assert_eq!(
            window.find("login-pair").label(),
            Some("Sign in by pairing: show a QR code to scan with your phone")
        );
        assert_eq!(
            window.find("login-github").label(),
            Some("Sign in with GitHub in your system browser")
        );
        assert_eq!(
            window.find("login-status").label(),
            Some("Pair this computer with your phone, or continue with GitHub.")
        );
        // Tab order is the order on screen: pairing first, GitHub second.
        let pair = window.find("login-pair").bounds();
        let github = window.find("login-github").bounds();
        assert!(pair.origin.x < github.origin.x);
    })
    .expect("window");
}

#[gpui_kit::test]
fn pairing_shows_a_qr_and_the_code_then_signs_in_and_signs_out(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["gate"], 600);
    let store = Arc::new(MemoryStore::default());
    let (window, view) = open_shell(cx, &hub, store.clone(), Arc::new(RecordingBrowser::new()));
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| {
            shell.start_login(LoginMethod::Pairing, cx);
        });
        window.render_frame(cx);
        assert_eq!(
            window.find("login-cancel").label(),
            Some("Cancel signing in")
        );
    })
    .expect("window");

    wait_until(cx, window, &view, |shell| {
        matches!(shell.model().login.stage(), LoginStage::Pairing { .. })
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let qr = window.find("login-qr");
        assert!(qr.visible());
        assert!(
            qr.bounds().size.width > px(80.),
            "the QR code has a real size"
        );
        assert!(
            qr.label()
                .expect("qr label")
                .starts_with("QR code for pairing")
        );
        assert_eq!(
            window.find("login-user-code").label(),
            Some("Pairing code ABCD-EFGH")
        );
        assert_eq!(view.read(cx).model().screen(), Screen::Login);
    })
    .expect("window");
    // The phone approves.
    state.approved.store(true, Ordering::SeqCst);

    wait_until(cx, window, &view, |shell| {
        shell.model().screen() == Screen::Main
    });
    assert!(state.polls.load(Ordering::SeqCst) >= 2);
    assert_eq!(
        store.load().expect("stored").grant.token,
        "sm_session_token"
    );

    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let slots = view.read(cx).model().focus_order();
        assert_eq!(slots.len(), Section::ALL.len());
        for slot in &slots {
            assert_eq!(
                window.find(slot.id.clone()).label(),
                Some(slot.label.as_str()),
                "{}",
                slot.id
            );
        }
        assert_eq!(
            window.find("nav-inbox").label(),
            Some("Inbox, current page")
        );
        assert_eq!(window.find("nav-agents").label(), Some("Go to Agents"));

        window.click("nav-settings", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("nav-settings").label(),
            Some("Settings, current page")
        );
        assert_eq!(
            window.find("account").label(),
            Some("Signed in as Ada in Acme")
        );
        window.click("sign-out", cx);
        window.render_frame(cx);
        assert!(window.find("login-pair").label().is_some());
    })
    .expect("window");
    assert!(store.load().is_none());
}

#[gpui_kit::test]
fn github_button_opens_the_system_browser_at_the_authorize_url(cx: &mut TestAppContext) {
    let browser = Arc::new(RecordingBrowser::new());
    let (window, view) = open_shell(
        cx,
        "https://hub.test",
        Arc::new(MemoryStore::default()),
        browser.clone(),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("login-github", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("login-cancel").label(),
            Some("Cancel signing in")
        );
    })
    .expect("window");
    wait_until(cx, window, &view, |shell| {
        matches!(shell.model().login.stage(), LoginStage::Browser)
    });
    let opened = browser.opened();
    assert_eq!(opened.len(), 1);
    assert!(opened[0].starts_with("https://hub.test/v1/auth/github/authorize?"));
    assert!(opened[0].contains("code_challenge_method=S256"));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("login-status").label(),
            Some(
                "Finish signing in with GitHub in your browser. This window continues on its own."
            )
        );
        window.click("login-cancel", cx);
        window.render_frame(cx);
        assert!(window.find("login-github").label().is_some());
    })
    .expect("window");
}

#[gpui_kit::test]
fn a_failed_sign_in_shows_the_error_and_offers_retry(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        "https://hub.test",
        Arc::new(MemoryStore::default()),
        Arc::new(RecordingBrowser::failing("no display")),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("login-github", cx);
    })
    .expect("window");
    wait_until(cx, window, &view, |shell| {
        matches!(shell.model().login.stage(), LoginStage::Failed(_))
    });
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.find("login-status").label(),
            Some("Could not open your browser: no display")
        );
        assert_eq!(
            window.find("login-retry").label(),
            Some("Go back and try signing in again")
        );
        window.click("login-retry", cx);
        window.render_frame(cx);
        assert!(window.try_find("login-pair").is_some());
    })
    .expect("window");
}

fn signed_in_store() -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::default());
    store
        .save(&Session {
            hub: "https://hub.test".into(),
            grant: grant(),
        })
        .expect("save");
    store
}

#[gpui_kit::test]
fn the_theme_switches_between_light_and_dark_from_settings(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        "https://hub.test",
        signed_in_store(),
        Arc::new(RecordingBrowser::new()),
    );
    let is_dark = |cx: &mut TestAppContext| cx.update(|cx| cx.theme().is_dark());
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("nav-settings", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("theme-system").label(),
            Some("Theme: Match system, selected")
        );
        window.click("theme-dark", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("theme-dark").label(),
            Some("Theme: Dark, selected")
        );
        assert_eq!(window.find("theme-light").label(), Some("Theme: Light"));
    })
    .expect("window");
    assert!(is_dark(cx));
    cx.update_window(window, |_, window, cx| {
        window.click("theme-light", cx);
        window.render_frame(cx);
    })
    .expect("window");
    assert!(!is_dark(cx));
    cx.update(|cx| assert_eq!(view.read(cx).model().prefs().theme, ThemeChoice::Light));
}

#[gpui_kit::test]
fn navigation_buttons_and_shortcuts_change_the_page(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        "https://hub.test",
        signed_in_store(),
        Arc::new(RecordingBrowser::new()),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(window.find("content").label(), Some("Page content"));
        window.click("nav-machines", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().nav.current(), Section::Machines);
        assert_eq!(
            window.find("nav-machines").label(),
            Some("Machines, current page")
        );
        assert_eq!(window.find("nav-inbox").label(), Some("Go to Inbox"));

        window.press("ctrl-n", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().nav.current(), Section::NewTask);
        window.press("ctrl-tab", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().nav.current(), Section::Agents);
        window.press("ctrl-shift-tab", cx);
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().nav.current(), Section::NewTask);
    })
    .expect("window");
}

#[gpui_kit::test]
fn the_hub_address_is_edited_in_settings_and_saved_on_enter(cx: &mut TestAppContext) {
    let dir = unique_dir("hub-settings");
    let prefs = dir.join("preferences.json");
    // The signed-in session's hub is the one in use at launch (no saved preference yet).
    let (window, view) = open_shell_at(
        cx,
        "https://hub.test",
        signed_in_store(),
        Arc::new(RecordingBrowser::new()),
        Some(prefs.clone()),
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("nav-settings", cx);
        window.render_frame(cx);
        assert_eq!(window.find("hub-address").label(), Some("Hub address"));
        let field = view.read(cx).hub_input().clone();
        assert_eq!(field.read(cx).value().as_ref(), "https://hub.test");
        let focus = field.read(cx).focus_handle(cx);
        window.focus(&focus, cx);
        // A refused address.
        field.update(cx, |input, cx| input.set_value("not a hub", window, cx));
        window.press("enter", cx);
    })
    .expect("window");
    // Events are delivered when the window update ends, so the outcome is checked after it.
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        let field = view.read(cx).hub_input().clone();
        // The hub in use stays, and the field shows it again.
        assert_eq!(view.read(cx).model().hub(), "https://hub.test");
        assert_eq!(field.read(cx).value().as_ref(), "https://hub.test");
        assert!(!prefs.exists(), "a refused address is not saved");

        // An accepted address takes effect.
        field.update(cx, |input, cx| {
            input.set_value("http://127.0.0.1:8787", window, cx)
        });
        window.press("enter", cx);
    })
    .expect("window");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().hub(), "http://127.0.0.1:8787");
    })
    .expect("window");
    assert_eq!(Preferences::load(&prefs).hub_url, "http://127.0.0.1:8787");
}

/// The connection banner's text as the window shows it now (pumps the foreground first).
fn connection_banner(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle) -> Option<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find("connectivity-banner")
            .and_then(|found| found.label().map(str::to_owned))
    })
    .expect("window")
}

/// Waits (with a deadline) until the connection banner reads `expected`. The banner follows a
/// watch feed, so a single read right after a state change can see the previous state.
fn wait_for_connection_banner(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    expected: Option<&str>,
) {
    let mut last = None;
    for _ in 0..400 {
        last = connection_banner(cx, window);
        if last.as_deref() == expected {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("connection banner never read {expected:?}; last {last:?}");
}

#[gpui_kit::test]
fn the_banner_follows_a_client_core_connection(cx: &mut TestAppContext) {
    // The window's shell runs its own session link on a real thread (like the link tests below);
    // without this the test scheduler flags that thread's wake-ups as non-deterministic.
    cx.executor().allow_parking();
    // Offline: the hub refuses the socket. Then it comes up, the subscription goes live, and
    // the outbox's first answer brings the whole connection online.
    let up = Arc::new(AtomicBool::new(false));
    let hub = spawn_socket_hub(up.clone());
    let runtime = tokio::runtime::Runtime::new().expect("client runtime");
    let mut cache = Cache::open_in_memory().expect("cache");
    let outbox = Outbox::load(&cache, RetryPolicy::default()).expect("outbox");
    let subscription = runtime
        .block_on(Subscription::connect_or_offline(
            &hub,
            ThreadSync::new("th_1"),
            None,
        ))
        .expect("an unreachable hub is not an error");
    let mut connection = Connection::new(subscription, outbox);
    assert_eq!(connection.connectivity(), Connectivity::Offline);

    let (window, view) = open_shell(
        cx,
        &hub,
        signed_in_store(),
        Arc::new(RecordingBrowser::new()),
    );
    let feed = connection.watch_connectivity();
    cx.update(|cx| view.update(cx, |shell, cx| shell.follow_connectivity(feed, cx)));
    wait_for_connection_banner(cx, window, Some(OFFLINE));

    up.store(true, Ordering::SeqCst);
    runtime
        .block_on(connection.reconnect())
        .expect("the hub accepts the socket");
    assert_eq!(connection.connectivity(), Connectivity::Connecting);
    wait_for_connection_banner(cx, window, Some("Reconnecting..."));

    let applied = runtime
        .block_on(connection.next_persisted(&mut cache))
        .expect("the live frame is read");
    assert_eq!(applied, Applied::Live);
    // Live with nothing queued: the idle outbox has no opinion, so the banner hides.
    assert_eq!(connection.connectivity(), Connectivity::Online);
    wait_for_connection_banner(cx, window, None);

    connection
        .enqueue(
            &cache,
            Intent::Cancel {
                thread_id: "th_1".to_owned(),
                task_id: "tk_1".to_owned(),
            },
            0,
        )
        .expect("queues");
    // A queued send the hub has not answered yet is still connecting.
    wait_for_connection_banner(cx, window, Some("Reconnecting..."));
    let report = runtime
        .block_on(connection.flush(
            &cache,
            &Answering,
            &hub,
            &ManualClock::new(0),
            &mut FixedRandom(0),
        ))
        .expect("flushes");
    assert_eq!(report.done, 1);
    assert_eq!(connection.connectivity(), Connectivity::Online);
    wait_for_connection_banner(cx, window, None);
}

#[gpui_kit::test]
fn the_offline_banner_is_announced_as_a_status(cx: &mut TestAppContext) {
    let (window, view) = open_shell(
        cx,
        "https://hub.test",
        signed_in_store(),
        Arc::new(RecordingBrowser::new()),
    );
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| {
            shell.set_connectivity(Connectivity::Offline, cx);
        });
        window.render_frame(cx);
        let banner = window.find("connectivity-banner");
        assert_eq!(banner.role(), Some(gpui_kit::Role::Status));
        assert!(
            banner
                .label()
                .expect("label")
                .starts_with("You are offline")
        );
        view.update(cx, |shell, cx| {
            shell.set_connectivity(Connectivity::Online, cx)
        });
        window.render_frame(cx);
        assert!(window.try_find("connectivity-banner").is_none());
    })
    .expect("window");
}

#[test]
fn a_hub_address_must_be_an_absolute_http_or_https_url() {
    assert_eq!(
        parse_hub("  https://hub.example.com  ").as_deref(),
        Some("https://hub.example.com")
    );
    assert_eq!(
        parse_hub("http://127.0.0.1:8787").as_deref(),
        Some("http://127.0.0.1:8787")
    );
    for bad in [
        "",
        "hub.example.com",
        "ftp://hub.example.com",
        "https://",
        "not a url",
    ] {
        assert_eq!(parse_hub(bad), None, "{bad:?}");
    }
}

#[test]
fn a_hub_address_is_saved_and_used_at_the_next_launch() {
    let dir = unique_dir("hub-prefs");
    let prefs = dir.join("preferences.json");
    let mut shell = model(Arc::new(MemoryStore::default()), Some(prefs.clone()));
    assert_eq!(shell.hub(), "https://hub.test");
    shell.set_hub("http://127.0.0.1:8787/").expect("saves");
    assert!(shell.set_hub("ftp://hub.example.com").is_err());
    assert!(shell.set_hub("hub.example.com").is_err());
    // A refused address changes nothing, in memory or on disk.
    assert_eq!(shell.hub(), "http://127.0.0.1:8787/");
    assert_eq!(Preferences::load(&prefs).hub_url, "http://127.0.0.1:8787/");
    let relaunched = ShellModel::new(
        Arc::new(MemoryStore::default()),
        Preferences::load(&prefs),
        Some(prefs),
        "https://hub.test",
    );
    assert_eq!(relaunched.hub(), "http://127.0.0.1:8787/");
}

#[test]
fn api_urls_have_exactly_one_slash() {
    assert_eq!(api_url("https://h/", "/auth/x"), "https://h/v1/auth/x");
    assert_eq!(api_url("https://h", "auth/x"), "https://h/v1/auth/x");
}

// ---- M3-002c: the session owns its connection; hub field feedback ----------------------------------

const OFFLINE: &str = "You are offline. Changes will be sent when the connection returns.";

/// The banner's text as the window shows it now.
fn banner_now(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
) -> Option<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| shell.pump_login(cx));
        window.render_frame(cx);
        window
            .try_find("connectivity-banner")
            .and_then(|found| found.label().map(str::to_owned))
    })
    .expect("window")
}

/// Wait (real time; the link runs on its own thread) until the banner reads `expected`.
fn wait_for_banner(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
    expected: Option<&str>,
) {
    let mut last = None;
    for _ in 0..400 {
        last = banner_now(cx, window, view);
        if last.as_deref() == expected {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("banner never read {expected:?}; last {last:?}");
}

fn stored_session_on(hub: &str) -> Arc<MemoryStore> {
    let store = Arc::new(MemoryStore::default());
    store
        .save(&Session {
            hub: hub.to_owned(),
            grant: grant(),
        })
        .expect("save");
    store
}

#[gpui_kit::test]
fn after_sign_in_the_banner_follows_the_sessions_own_connection(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    // The session link runs on its own thread, so the test scheduler must tolerate real I/O.
    cx.executor().allow_parking();
    let (window, view) = open_shell(
        cx,
        &hub,
        Arc::new(MemoryStore::default()),
        Arc::new(RecordingBrowser::new()),
    );
    cx.update(|cx| assert!(view.read(cx).link().is_none(), "no session, no link"));
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.start_login(LoginMethod::Pairing, cx));
    })
    .expect("window");
    wait_until(cx, window, &view, |shell| {
        shell.model().screen() == Screen::Main
    });

    // The hub refuses the socket: the real connection is offline, and the banner says so.
    wait_for_banner(cx, window, &view, Some(OFFLINE));
    cx.update(|cx| {
        let shell = view.read(cx);
        assert_eq!(shell.link().expect("a link").hub(), hub);
    });
    // It subscribed with the session's own token.
    let bearers = state.bearers.lock().expect("lock").clone();
    assert!(!bearers.is_empty());
    assert!(
        bearers
            .iter()
            .all(|bearer| bearer == "Bearer sm_session_token")
    );

    // The hub accepts the socket: the connection retries and goes live. Nothing is queued, so
    // the idle outbox has no opinion and the banner goes away.
    state.socket_up.store(true, Ordering::SeqCst);
    wait_for_banner(cx, window, &view, None);

    // A send gets an answer: the connection stays online and the banner stays away.
    cx.update(|cx| {
        view.read(cx)
            .link()
            .expect("a link")
            .enqueue(Intent::Cancel {
                thread_id: "th_1".to_owned(),
                task_id: "tk_1".to_owned(),
            });
    });
    wait_until_true("the cancel to reach the hub", || {
        state.cancels.load(Ordering::SeqCst) == 1
    });
    assert_eq!(state.cancels.load(Ordering::SeqCst), 1);
    wait_for_banner(cx, window, &view, None);
}

#[gpui_kit::test]
fn a_stored_session_opens_the_link_at_launch_and_signing_out_closes_it(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    // The session link runs on its own thread, so the test scheduler must tolerate real I/O.
    cx.executor().allow_parking();
    state.socket_up.store(true, Ordering::SeqCst);
    let (window, view) = open_shell(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
    );
    // The socket is accepted and nothing is queued, so the banner hides once the link is live.
    wait_for_banner(cx, window, &view, None);
    cx.update(|cx| assert!(view.read(cx).link().is_some()));

    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.sign_out(cx));
    })
    .expect("window");
    cx.update(|cx| {
        assert!(
            view.read(cx).link().is_none(),
            "the grant is gone, so is the link"
        )
    });
}

#[gpui_kit::test]
fn the_hub_field_answers_a_save_with_a_message_and_a_different_hub_signs_out(
    cx: &mut TestAppContext,
) {
    let dir = unique_dir("hub-feedback");
    let prefs = dir.join("preferences.json");
    let store = stored_session_on("https://hub.test");
    let (window, view) = open_shell_at(
        cx,
        "https://hub.test",
        store.clone(),
        Arc::new(RecordingBrowser::new()),
        Some(prefs.clone()),
    );
    let feedback = |cx: &mut TestAppContext| -> Option<String> {
        cx.update_window(window, |_, window, cx| {
            window.render_frame(cx);
            window
                .try_find("hub-feedback")
                .and_then(|found| found.label().map(str::to_owned))
        })
        .expect("window")
    };
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window.click("nav-settings", cx);
        window.render_frame(cx);
        assert_eq!(
            window.find("hub-save").label(),
            Some("Save the hub address")
        );
        assert!(window.try_find("hub-feedback").is_none());
        // A refused address, saved with the Save control (not Enter).
        let field = view.read(cx).hub_input().clone();
        field.update(cx, |input, cx| input.set_value("not a hub", window, cx));
        window.click("hub-save", cx);
    })
    .expect("window");
    assert_eq!(
        feedback(cx).as_deref(),
        Some("Not saved: enter a full http or https address, for example https://hub.example.com.")
    );
    cx.update(|cx| {
        let field = view.read(cx).hub_input().clone();
        assert_eq!(field.read(cx).value().as_ref(), "https://hub.test");
    });
    assert!(!prefs.exists());

    // The session's own hub again: saved, still signed in.
    cx.update_window(window, |_, window, cx| {
        let field = view.read(cx).hub_input().clone();
        field.update(cx, |input, cx| {
            input.set_value("https://hub.test/", window, cx)
        });
        window.click("hub-save", cx);
    })
    .expect("window");
    assert_eq!(feedback(cx).as_deref(), Some("Hub address saved."));
    assert!(store.load().is_some());
    cx.update(|cx| assert_eq!(view.read(cx).model().screen(), Screen::Main));

    // Another hub: saved, signed out, and the login screen says why.
    cx.update_window(window, |_, window, cx| {
        let field = view.read(cx).hub_input().clone();
        field.update(cx, |input, cx| {
            input.set_value("http://127.0.0.1:9", window, cx)
        });
        window.click("hub-save", cx);
    })
    .expect("window");
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(view.read(cx).model().screen(), Screen::Login);
        assert_eq!(
            window.find("login-notice").label(),
            Some(
                "You were signed out because the hub address changed to http://127.0.0.1:9. \
                 Sign in again to use it."
            )
        );
        assert!(view.read(cx).link().is_none());
    })
    .expect("window");
    assert!(store.load().is_none(), "the old hub's session is forgotten");
    assert_eq!(Preferences::load(&prefs).hub_url, "http://127.0.0.1:9");

    // Starting a new sign-in clears the notice.
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| shell.start_login(LoginMethod::Pairing, cx));
        window.render_frame(cx);
        assert!(window.try_find("login-notice").is_none());
    })
    .expect("window");
}

#[test]
fn saving_the_hub_while_signed_in_signs_out_only_for_another_hub() {
    let dir = unique_dir("hub-model");
    let prefs = dir.join("preferences.json");
    let store = stored_session_on("https://hub.test");
    let mut shell = ShellModel::new(
        store.clone(),
        Preferences::default(),
        Some(prefs),
        "https://hub.test",
    );
    assert_eq!(shell.change_hub("nope"), HubOutcome::Refused);
    assert_eq!(shell.screen(), Screen::Main);
    assert_eq!(shell.change_hub(" https://hub.test/ "), HubOutcome::Saved);
    assert_eq!(shell.screen(), Screen::Main);
    assert_eq!(
        shell.change_hub("https://other.test"),
        HubOutcome::SavedSignedOut
    );
    assert_eq!(shell.screen(), Screen::Login);
    assert!(store.load().is_none());
    assert_eq!(shell.hub(), "https://other.test");
    // Signed out, a different hub is just saved.
    assert_eq!(shell.change_hub("https://third.test"), HubOutcome::Saved);

    // A preferences file that cannot be written is reported, not hidden.
    let blocker = dir.join("blocker");
    std::fs::write(&blocker, "a file, not a directory").expect("blocker");
    let mut broken = ShellModel::new(
        Arc::new(MemoryStore::default()),
        Preferences::default(),
        Some(blocker.join("preferences.json")),
        "https://hub.test",
    );
    assert_eq!(broken.change_hub("https://a.test"), HubOutcome::NotStored);
}

// ---- approval cards in the app connection (M3-011c) -----------------------------------------

/// 2026-10-27T22:00:16Z, when the example interrupt is raised (its `expires_at` is ten minutes on).
const RAISED_MS: u64 = 1_793_138_416_000;

fn event_frame(seq: u64, thread: &str, event: Value) -> String {
    json!({
        "type": "event",
        "envelope": {
            "v": 1, "seq": seq, "thread_id": thread, "task_id": "tk_1", "run_id": "run_1",
            "ts": "2026-10-27T22:00:16.000Z", "event": event,
        }
    })
    .to_string()
}

/// The hub replays one pending approval for `th_a`.
fn hub_with_pending_approval() -> (Arc<HubState>, String) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    state.frames.lock().expect("lock").insert(
        "th_a".to_owned(),
        vec![event_frame(
            1,
            "th_a",
            json!({
                "type": "Interrupt", "interrupt_id": "int_1", "kind": "approval",
                "title": "Allow edit to src/main.rs?", "message": "The agent wants to change a file",
                "expires_at": "2026-10-27T22:10:00.000Z", "tool_call_id": "tc_1",
                "options": [
                    { "option_id": "yes_once", "name": "Allow", "kind": "allow_once" },
                    { "option_id": "nope", "name": "Deny", "kind": "reject_once" },
                ],
            }),
        )],
    );
    (state, hub)
}

/// Renders and returns the label of the element `id`, if the window shows one.
fn label_of(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    id: &'static str,
) -> Option<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .try_find(id)
            .and_then(|found| found.label().map(str::to_owned))
    })
    .expect("window")
}

/// Waits (real time; the link runs on its own thread) until `id` has a label satisfying `ok`.
fn wait_for_label(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    id: &'static str,
    ok: impl Fn(&str) -> bool,
) -> String {
    let mut last = None;
    for _ in 0..400 {
        last = label_of(cx, window, id);
        if let Some(label) = last.as_deref().filter(|label| ok(label)) {
            return label.to_owned();
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never matched; last {last:?}");
}

fn open_thread_a(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
) {
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| {
            shell.open_thread("th_a", Some("/work/repo"), cx)
        });
    })
    .expect("window");
}

/// Pumps the window until `id` is laid out (the thread's snapshot arrives on the link's thread).
fn wait_for_shown(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle, id: &'static str) {
    for _ in 0..400 {
        cx.run_until_parked();
        let shown = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_some()
            })
            .expect("window");
        if shown {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never shown");
}

fn wait_until_true(what: &str, done: impl Fn() -> bool) {
    for _ in 0..400 {
        if done() {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("timed out waiting for {what}");
}

/// A row of the inbox's list (`GET /v1/threads`).
fn inbox_row(id: &str, title: &str, status: &str, workspace: &str) -> Value {
    json!({
        "thread_id": id, "title": title, "machine_id": "mch_1", "agent": "claude",
        "status": status, "updated_at": "2026-10-28T10:00:00.000Z", "last_seq": 7,
        "workspace": workspace,
    })
}

/// The same row with the open approval the hub reports as `pending_interrupts`.
fn with_approval(mut row: Value) -> Value {
    row["pending_interrupts"] = json!([{
        "interrupt_id": "int_1", "title": "Allow edit to src/main.rs?",
        "options": [
            { "option_id": "yes_once", "name": "Yes", "kind": "allow_once" },
            { "option_id": "nope", "name": "No", "kind": "reject_once" },
        ],
    }]);
    row
}

/// Pumps the inbox (its refresh timer runs on the test clock) until `id` is laid out.
fn wait_for_inbox(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle, id: &'static str) {
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        let shown = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_some()
            })
            .expect("window");
        if shown {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never shown in the inbox");
}

#[gpui_kit::test]
fn clicking_an_inbox_row_shows_its_thread_and_follows_it(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    *state.threads.lock().expect("lock") =
        vec![inbox_row("th_a", "Fix login", "running", "/work/repo")];
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    wait_for_inbox(cx, window, "inbox-open-th_a");
    assert!(state.subscribed.lock().expect("lock").is_empty());

    cx.update_window(window, |_, window, cx| {
        window.click("inbox-open-th_a", cx);
    })
    .expect("window");
    wait_until_true("a subscription to th_a", || {
        *state.subscribed.lock().expect("lock") == ["th_a"]
    });
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_a")));
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(
            window.try_find("thread-back").is_some(),
            "the thread page is shown"
        );
    })
    .expect("window");
}

#[gpui_kit::test]
fn quick_allow_on_a_row_answers_and_does_not_open_its_thread(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    *state.threads.lock().expect("lock") = vec![
        with_approval(inbox_row("th_a", "Needs me", "needs_you", "/work/repo")),
        inbox_row("th_b", "Fix login", "running", "/work/other"),
    ];
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    // Open th_b from its row, then go back to the list.
    wait_for_inbox(cx, window, "inbox-open-th_b");
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-open-th_b", cx);
    })
    .expect("window");
    wait_until_true("a subscription to th_b", || {
        *state.subscribed.lock().expect("lock") == ["th_b"]
    });
    cx.update_window(window, |_, window, cx| {
        window.click("thread-back", cx);
    })
    .expect("window");

    // Allow on th_a answers the approval and must not open th_a.
    wait_for_inbox(cx, window, "inbox-allow-th_a");
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-allow-th_a", cx);
    })
    .expect("window");
    wait_until_true("the quick answer to reach the hub", || {
        !state.answers.lock().expect("lock").is_empty()
    });
    let answers = state.answers.lock().expect("lock").clone();
    assert_eq!(answers[0].thread, "th_a");
    assert_eq!(answers[0].interrupt, "int_1");
    // No command reached the link for th_a (the click handler is synchronous and the shell's
    // open thread is still unset), so the subscription list cannot grow past th_b.
    assert_eq!(*state.subscribed.lock().expect("lock"), ["th_b"]);
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), None));
}

#[gpui_kit::test]
fn the_link_follows_the_thread_the_screens_open(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_pending_approval();
    cx.executor().allow_parking();
    // No thread yet: the placeholder `th_session` is gone, nothing is subscribed.
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    let _ = banner_now(cx, window, &view);
    assert!(state.subscribed.lock().expect("lock").is_empty());
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), None));

    open_thread_a(cx, window, &view);
    wait_until_true("a subscription to th_a", || {
        *state.subscribed.lock().expect("lock") == ["th_a"]
    });
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_a")));

    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.open_thread("th_b", None, cx));
    })
    .expect("window");
    wait_until_true("a subscription to th_b", || {
        state
            .subscribed
            .lock()
            .expect("lock")
            .iter()
            .any(|thread| thread == "th_b")
    });
    // Opening the thread already followed is not a new subscription. The link handles its
    // commands in order, so a real switch to th_a after the no-op is a barrier: any extra
    // subscription the no-op caused would show up before it in the list.
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.open_thread("th_b", None, cx));
    })
    .expect("window");
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.open_thread("th_a", None, cx));
    })
    .expect("window");
    wait_until_true("the switch back to th_a to be followed", || {
        state.subscribed.lock().expect("lock").len() >= 3
    });
    assert_eq!(
        *state.subscribed.lock().expect("lock"),
        ["th_a", "th_b", "th_a"]
    );
}

#[gpui_kit::test]
fn an_open_thread_shows_its_pending_card_and_a_click_posts_the_options_own_id(
    cx: &mut TestAppContext,
) {
    let (state, hub) = hub_with_pending_approval();
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    open_thread_a(cx, window, &view);
    let title = wait_for_label(cx, window, "approval-title", |label| !label.is_empty());
    assert_eq!(title, "Allow edit to src/main.rs?");
    assert!(label_of(cx, window, "approval-option-yes_once").is_some());
    assert!(label_of(cx, window, "approval-option-nope").is_some());

    cx.update_window(window, |_, window, cx| {
        window.click("approval-option-yes_once", cx)
    })
    .expect("window");
    wait_until_true("the answer to reach the hub", || {
        !state.answers.lock().expect("lock").is_empty()
    });
    let answers = state.answers.lock().expect("lock").clone();
    assert_eq!(answers.len(), 1, "{answers:?}");
    let answer = &answers[0];
    assert_eq!(answer.thread, "th_a");
    assert_eq!(answer.interrupt, "int_1");
    assert_eq!(
        answer.body["outcome"],
        json!({ "type": "selected", "option_id": "yes_once" }),
        "the option's own id, not a verdict"
    );
    let key = answer.key.as_deref().expect("an Idempotency-Key");
    assert!(key.starts_with("idk_"), "{key}");
}

/// The hub replays one complete diff of `src/lib.rs` for `th_a`: twenty numbered lines with
/// line 12 changed (the same fixture as the transcript tests).
fn hub_with_a_diff() -> (Arc<HubState>, String) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    let old = (0..20)
        .map(|n| format!("line {n}"))
        .collect::<Vec<_>>()
        .join("\n")
        + "\n";
    let new = old.replace("line 12\n", "twelve\n");
    state.frames.lock().expect("lock").insert(
        "th_a".to_owned(),
        vec![event_frame(
            1,
            "th_a",
            json!({
                "type": "Activity", "activity_id": "d", "activity_type": "diff",
                "mode": "snapshot",
                "content": { "path": "src/lib.rs", "old_text": old, "new_text": new },
            }),
        )],
    );
    (state, hub)
}

#[gpui_kit::test]
fn a_diff_in_a_thread_with_a_workspace_asks_the_shell_to_open_its_file(cx: &mut TestAppContext) {
    let (_state, hub) = hub_with_a_diff();
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    // open_thread_a opens th_a with the workspace /work/repo.
    open_thread_a(cx, window, &view);
    wait_for_shown(cx, window, VIEW_DIFF_ID);
    cx.update_window(window, |_, window, cx| window.click(VIEW_DIFF_ID, cx))
        .expect("window");
    wait_for_shown(cx, window, DIFF_OPEN_ID);
    assert!(cx.update(|cx| view.read(cx).last_open_file().is_none()));

    cx.update_window(window, |_, window, cx| window.click(DIFF_OPEN_ID, cx))
        .expect("window");
    cx.update(|cx| {
        assert_eq!(
            view.read(cx).last_open_file(),
            Some((std::path::Path::new("/work/repo/src/lib.rs"), 13))
        );
    });
}

#[gpui_kit::test]
fn a_diff_in_a_thread_without_a_workspace_offers_no_open_file(cx: &mut TestAppContext) {
    let (_state, hub) = hub_with_a_diff();
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.open_thread("th_a", None, cx));
    })
    .expect("window");
    wait_for_shown(cx, window, VIEW_DIFF_ID);
    cx.update_window(window, |_, window, cx| window.click(VIEW_DIFF_ID, cx))
        .expect("window");
    wait_for_shown(cx, window, DIFF_PANE_ID);
    cx.update_window(window, |_, window, _| {
        assert!(window.find_all(DIFF_OPEN_ID).is_empty());
    })
    .expect("window");
    assert!(cx.update(|cx| view.read(cx).last_open_file().is_none()));
}

/// The deadline of the card `hub_with_pending_approval` raises (2026-10-27T22:10:00Z).
const EXPIRES_MS: u64 = 1_793_139_000_000;

#[gpui_kit::test]
fn a_click_refused_as_expired_says_so_on_the_card(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_pending_approval();
    cx.executor().allow_parking();
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        clock.clone(),
    );
    open_thread_a(cx, window, &view);
    wait_for_label(cx, window, "approval-title", |label| !label.is_empty());

    // The deadline passes before the next refresh, so the card still offers its answers.
    clock.set(EXPIRES_MS);
    cx.update_window(window, |_, window, cx| {
        window.click("approval-option-yes_once", cx)
    })
    .expect("window");
    let status = wait_for_label(cx, window, "approval-status", |label| {
        label.contains("expired")
    });
    assert_eq!(
        status,
        "Your answer was not delivered: the request expired before your answer was sent"
    );
    assert!(
        state.answers.lock().expect("lock").is_empty(),
        "a refused answer never reaches the hub"
    );
}

#[gpui_kit::test]
fn time_left_repaints_every_second(cx: &mut TestAppContext) {
    let (_state, hub) = hub_with_pending_approval();
    cx.executor().allow_parking();
    let clock = Arc::new(ManualClock::new(RAISED_MS));
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        clock.clone(),
    );
    open_thread_a(cx, window, &view);
    let first = wait_for_label(cx, window, "approval-time-left", |label| !label.is_empty());

    // One second passes on the app clock and on the repaint timer.
    clock.advance(1_000);
    cx.executor().advance_clock(Duration::from_secs(1));
    let second = wait_for_label(cx, window, "approval-time-left", |label| label != first);
    assert_ne!(first, second);

    clock.advance(1_000);
    cx.executor().advance_clock(Duration::from_secs(1));
    let third = wait_for_label(cx, window, "approval-time-left", |label| label != second);
    assert_ne!(second, third);
}

/// The labels of every element `id` the window shows, after a frame.
fn labels_shown(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    id: &'static str,
) -> Vec<String> {
    cx.run_until_parked();
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        window
            .find_all(id)
            .iter()
            .map(|found| found.label().unwrap_or_default().to_owned())
            .collect()
    })
    .expect("window")
}

fn plan_frame(seq: u64, entries: Value) -> String {
    event_frame(
        seq,
        "th_a",
        json!({
            "type": "Activity", "activity_id": "plan", "activity_type": "plan",
            "mode": "snapshot", "content": { "entries": entries },
        }),
    )
}

fn usage_frame(seq: u64, document: Value) -> String {
    event_frame(
        seq,
        "th_a",
        json!({ "type": "StateSnapshot", "scope": "usage", "snapshot": document }),
    )
}

#[gpui_kit::test]
fn an_open_thread_shows_its_plan_and_usage_and_follows_new_events(cx: &mut TestAppContext) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    state.frames.lock().expect("lock").insert(
        "th_a".to_owned(),
        vec![
            plan_frame(
                1,
                json!([
                    { "content": "Add route", "priority": "high", "status": "in_progress" },
                    { "content": "Add test", "priority": "medium", "status": "pending" },
                ]),
            ),
            usage_frame(
                2,
                json!({ "context_used": 10_000, "context_size": 200_000,
                        "cost": { "amount": 0.01, "currency": "USD" } }),
            ),
        ],
    );
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    open_thread_a(cx, window, &view);

    // The initial frame: the plan's rows and the usage figures.
    assert_eq!(
        wait_for_label(cx, window, PLAN_SUMMARY_ID, |label| !label.is_empty()),
        "0 of 2 done"
    );
    assert_eq!(
        labels_shown(cx, window, PLAN_ROW_ID),
        [
            "Add route: In progress, High priority",
            "Add test: Pending, Medium priority"
        ]
    );
    assert_eq!(
        wait_for_label(cx, window, TOTAL_TOKENS_ID, |label| !label.is_empty()),
        "Total: 10k tokens"
    );
    assert_eq!(
        labels_shown(cx, window, TURN_TOKENS_ID),
        ["This turn: 10k tokens"]
    );
    assert_eq!(labels_shown(cx, window, TOTAL_COST_ID), ["Total: 0.01 USD"]);

    // The hub sends a new plan and a new usage state; both widgets change.
    push_live(
        &state,
        "th_a",
        plan_frame(
            3,
            json!([
                { "content": "Add route", "priority": "high", "status": "completed" },
                { "content": "Add test", "priority": "medium", "status": "in_progress" },
            ]),
        ),
    );
    push_live(
        &state,
        "th_a",
        usage_frame(
            4,
            json!({ "context_used": 30_000, "context_size": 200_000,
                    "cost": { "amount": 0.03, "currency": "USD" } }),
        ),
    );
    assert_eq!(
        wait_for_label(cx, window, PLAN_SUMMARY_ID, |label| label == "1 of 2 done"),
        "1 of 2 done"
    );
    assert_eq!(
        labels_shown(cx, window, PLAN_ROW_ID),
        [
            "Add route: Completed, High priority",
            "Add test: In progress, Medium priority"
        ]
    );
    assert_eq!(
        wait_for_label(cx, window, TOTAL_TOKENS_ID, |label| label
            == "Total: 30k tokens"),
        "Total: 30k tokens"
    );
    assert_eq!(
        labels_shown(cx, window, TURN_TOKENS_ID),
        ["This turn: 30k tokens"]
    );
    assert_eq!(labels_shown(cx, window, TOTAL_COST_ID), ["Total: 0.03 USD"]);
}

#[gpui_kit::test]
fn a_thread_without_a_plan_or_usage_shows_the_widgets_empty_states(cx: &mut TestAppContext) {
    let (_state, hub) = hub_with_a_diff();
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    open_thread_a(cx, window, &view);
    wait_for_shown(cx, window, VIEW_DIFF_ID);

    assert_eq!(
        labels_shown(cx, window, PLAN_EMPTY_ID),
        ["No plan yet"],
        "the plan widget says there is no plan"
    );
    assert!(labels_shown(cx, window, PLAN_ROW_ID).is_empty());
    assert_eq!(labels_shown(cx, window, USAGE_ID), ["Usage"]);
    for id in [
        TURN_TOKENS_ID,
        TOTAL_TOKENS_ID,
        TURN_COST_ID,
        TOTAL_COST_ID,
        CONTEXT_ID,
    ] {
        assert!(
            labels_shown(cx, window, id).is_empty(),
            "{id} shows nothing without usage"
        );
    }
}

// ---- M3-005a: the composer in the open thread --------------------------------------------------

fn state_frame(seq: u64, scope: &str, snapshot: Value) -> String {
    event_frame(
        seq,
        "th_a",
        json!({ "type": "StateSnapshot", "scope": scope, "snapshot": snapshot }),
    )
}

fn run_started_frame(seq: u64) -> String {
    event_frame(
        seq,
        "th_a",
        json!({ "type": "RunStarted", "reason": "prompt", "agent_id": "agt_claude",
                "machine_id": "mch_laptop" }),
    )
}

/// `th_a` has a session (two commands, three modes, current `default`), optionally runs a task
/// and has `tk_q` queued.
fn hub_with_a_session(running: bool) -> (Arc<HubState>, String) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    let mut frames = vec![
        state_frame(
            1,
            "session",
            json!({
                "available_commands": [
                    { "name": "compact", "description": "Summarize" },
                    { "name": "review", "description": "Review changes" },
                ],
                "modes": [
                    { "id": "plan", "name": "Plan" },
                    { "id": "default", "name": "Default" },
                    { "id": "bypassPermissions", "name": "Bypass" },
                ],
                "current_mode_id": "default",
            }),
        ),
        state_frame(
            2,
            "thread",
            json!({ "pending_tasks": [{ "task_id": "tk_q", "preview": "then add tests" }] }),
        ),
    ];
    frames.push(if running {
        run_started_frame(3)
    } else {
        // A task that ran and finished: nothing is running.
        event_frame(
            3,
            "th_a",
            json!({ "type": "RunFinished",
                    "outcome": { "type": "success", "stop_reason": "end_turn" } }),
        )
    });
    state
        .frames
        .lock()
        .expect("lock")
        .insert("th_a".to_owned(), frames);
    (state, hub)
}

fn open_composer_shell(
    cx: &mut TestAppContext,
    hub: &str,
) -> (gpui_kit::AnyWindowHandle, Entity<AppShell>) {
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        hub,
        stored_session_on(hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    open_thread_a(cx, window, &view);
    (window, view)
}

fn writes_to(state: &HubState, method: &str, suffix: &str) -> Vec<Write> {
    state
        .writes
        .lock()
        .expect("lock")
        .iter()
        .filter(|write| write.method == method && write.path.ends_with(suffix))
        .cloned()
        .collect()
}

fn type_into(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
    text: &'static str,
) {
    cx.update_window(window, |_, window, cx| {
        let input = view
            .read(cx)
            .thread_composer()
            .read(cx)
            .prompt_input()
            .clone();
        input.update(cx, |state, cx| state.set_value(text, window, cx));
    })
    .expect("window");
}

#[gpui_kit::test]
fn the_open_thread_shows_a_composer_that_follows_each_snapshot(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    let (window, view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-composer");
    // The session's modes came from the hub's state (set_thread on the snapshot).
    wait_for_shown(cx, window, "thread-mode-plan");
    // A slash opens the list with the session's commands.
    type_into(cx, window, &view, "/");
    wait_for_shown(cx, window, "slash-compact");
    assert!(label_of(cx, window, "slash-review").is_some());
    // A later snapshot reaches the composer: the hub replaces the commands.
    push_live(
        &state,
        "th_a",
        state_frame(
            4,
            "session",
            json!({
                "available_commands": [{ "name": "init", "description": "Create AGENTS" }],
                "modes": [{ "id": "default", "name": "Default" }],
                "current_mode_id": "default",
            }),
        ),
    );
    wait_for_shown(cx, window, "slash-init");
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("slash-compact").is_none());
    })
    .expect("window");
}

#[gpui_kit::test]
fn sending_a_prompt_goes_through_the_outbox_with_fresh_ids(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    let (window, view) = open_composer_shell(cx, &hub);
    // The composer has the thread once the first snapshot arrived (its modes show).
    wait_for_shown(cx, window, "thread-mode-plan");
    type_into(cx, window, &view, "add a test please");
    cx.update_window(window, |_, window, cx| window.click("thread-send", cx))
        .expect("window");
    wait_until_true("the prompt to reach the hub", || {
        !writes_to(&state, "POST", "/prompts").is_empty()
    });
    let prompts = writes_to(&state, "POST", "/prompts");
    assert_eq!(prompts.len(), 1, "{prompts:?}");
    assert_eq!(prompts[0].path, "/v1/threads/th_a/prompts");
    assert!(
        prompts[0]
            .key
            .as_deref()
            .is_some_and(|k| k.starts_with("idk_"))
    );
    assert!(
        prompts[0].body["task_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("tk_"))
    );
    assert!(
        prompts[0].body["message_id"]
            .as_str()
            .is_some_and(|id| id.starts_with("msg_"))
    );
    assert_eq!(prompts[0].body["parts"][0]["text"], "add a test please");
    // The field is empty again.
    cx.update_window(window, |_, _, cx| {
        let input = view
            .read(cx)
            .thread_composer()
            .read(cx)
            .prompt_input()
            .clone();
        assert_eq!(input.read(cx).value().as_ref(), "");
    })
    .expect("window");
}

#[gpui_kit::test]
fn stop_while_a_task_runs_cancels_it_through_the_outbox(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(true);
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-stop");
    cx.update_window(window, |_, window, cx| window.click("thread-stop", cx))
        .expect("window");
    wait_until_true("the cancel to reach the hub", || {
        !writes_to(&state, "POST", "/cancel").is_empty()
    });
    let cancels = writes_to(&state, "POST", "/cancel");
    assert_eq!(cancels[0].path, "/v1/threads/th_a/cancel");
    assert_eq!(cancels[0].body["task_id"], "tk_1");
    assert!(
        cancels[0]
            .key
            .as_deref()
            .is_some_and(|k| k.starts_with("idk_"))
    );
}

#[gpui_kit::test]
fn a_mode_change_and_removing_a_queued_chip_hit_the_hub_with_keys(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(true);
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-plan");
    cx.update_window(window, |_, window, cx| window.click("thread-mode-plan", cx))
        .expect("window");
    wait_until_true("the mode change", || {
        !writes_to(&state, "POST", "/session").is_empty()
    });
    let sessions = writes_to(&state, "POST", "/session");
    assert_eq!(sessions[0].path, "/v1/threads/th_a/session");
    assert_eq!(sessions[0].body["mode_id"], "plan");
    let mode_key = sessions[0].key.clone().expect("a key");
    assert!(mode_key.starts_with("idk_"));

    wait_for_shown(cx, window, "queued-remove-tk_q");
    cx.update_window(window, |_, window, cx| {
        window.click("queued-remove-tk_q", cx)
    })
    .expect("window");
    wait_until_true("the removal", || {
        !writes_to(&state, "DELETE", "/pending/tk_q").is_empty()
    });
    let removals = writes_to(&state, "DELETE", "/pending/tk_q");
    assert_eq!(removals[0].path, "/v1/threads/th_a/pending/tk_q");
    let key = removals[0].key.clone().expect("a key");
    assert!(key.starts_with("idk_"));
    assert_ne!(key, mode_key);
}

#[gpui_kit::test]
fn a_409_task_started_on_remove_offers_stop_for_that_task(cx: &mut TestAppContext) {
    // No RunStarted frame: the thread's events do not show the task running, only the 409 does.
    let (state, hub) = hub_with_a_session(false);
    state.delete_status.store(409, Ordering::SeqCst);
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "queued-remove-tk_q");
    cx.update_window(window, |_, window, _| {
        assert!(window.try_find("thread-stop").is_none(), "no Stop before");
    })
    .expect("window");
    cx.update_window(window, |_, window, cx| {
        window.click("queued-remove-tk_q", cx)
    })
    .expect("window");
    wait_for_shown(cx, window, "thread-stop");
    wait_for_shown(cx, window, "thread-started-notice");
    cx.update_window(window, |_, window, cx| window.click("thread-stop", cx))
        .expect("window");
    wait_until_true("the cancel for tk_q", || {
        !writes_to(&state, "POST", "/cancel").is_empty()
    });
    assert_eq!(
        writes_to(&state, "POST", "/cancel")[0].body["task_id"],
        "tk_q"
    );
}

#[gpui_kit::test]
fn the_runner_cap_comes_from_the_hosting_machines_features(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(true);
    state.machines.lock().expect("lock").push(json!({
        "machine_id": "mch_laptop", "name": "Laptop",
        "presence": { "state": "online" },
        "capabilities": { "features": ["permission_mode:acceptEdits"] },
    }));
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-default");
    // `bypassPermissions` is looser than the machine's `acceptEdits`, so it is not offered.
    let mut gone = false;
    for _ in 0..400 {
        // The machines list is polled by a 50 ms timer on the test clock.
        cx.executor().advance_clock(Duration::from_millis(60));
        cx.run_until_parked();
        gone = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find("thread-mode-bypassPermissions").is_none()
            })
            .expect("window");
        if gone {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    assert!(gone, "the cap never applied");
    assert!(label_of(cx, window, "thread-mode-plan").is_some());
}

#[gpui_kit::test]
fn without_machine_features_every_mode_is_offered(cx: &mut TestAppContext) {
    let (_state, hub) = hub_with_a_session(true);
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-bypassPermissions");
}

fn task_event_frame(seq: u64, task: &str, event: Value) -> String {
    let mut frame: Value = serde_json::from_str(&event_frame(seq, "th_a", event)).expect("frame");
    frame["envelope"]["task_id"] = json!(task);
    frame.to_string()
}

/// Pumps the window until `id` is no longer laid out.
fn wait_for_gone(cx: &mut TestAppContext, window: gpui_kit::AnyWindowHandle, id: &'static str) {
    for _ in 0..400 {
        cx.run_until_parked();
        let gone = cx
            .update_window(window, |_, window, cx| {
                window.render_frame(cx);
                window.try_find(id).is_none()
            })
            .expect("window");
        if gone {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("{id} never went away");
}

#[gpui_kit::test]
fn refused_mode_change_and_chip_removal_are_named_on_the_composer(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    state.delete_status.store(403, Ordering::SeqCst);
    state.session_status.store(400, Ordering::SeqCst);
    let (window, _view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-plan");
    cx.update_window(window, |_, window, cx| window.click("thread-mode-plan", cx))
        .expect("window");
    wait_for_shown(cx, window, "thread-refused-0");
    let text = label_of(cx, window, "thread-refused-0").expect("label");
    assert!(text.contains("mode"), "{text}");
    assert!(text.contains("The request was not valid."), "{text}");
    assert!(!text.contains("invalid_request"), "{text}");

    cx.update_window(window, |_, window, cx| {
        window.click("queued-remove-tk_q", cx)
    })
    .expect("window");
    wait_for_shown(cx, window, "thread-refused-1");
    let text = label_of(cx, window, "thread-refused-1").expect("label");
    assert!(text.contains("remove"), "{text}");
    assert!(text.contains("You are not allowed to do that."), "{text}");
    assert!(!text.contains("forbidden"), "{text}");

    // The next successful request clears both.
    state.session_status.store(0, Ordering::SeqCst);
    cx.update_window(window, |_, window, cx| {
        window.click("thread-mode-default", cx)
    })
    .expect("window");
    wait_for_gone(cx, window, "thread-refused-0");
    wait_for_gone(cx, window, "thread-refused-1");
}

/// How many outbox entries are `Rejected`, read on the link thread.
fn rejected_entries(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
) -> usize {
    cx.update_window(window, |_, _, cx| {
        view.read(cx).link().expect("link").with_thread(|access| {
            access
                .outbox
                .entries()
                .filter(|entry| matches!(entry.state, client_core::IntentState::Rejected { .. }))
                .count()
        })
    })
    .expect("window")
    .recv()
    .expect("outbox")
}

fn wait_for_rejected(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
    want: usize,
) {
    let mut seen = rejected_entries(cx, window, view);
    for _ in 0..400 {
        if seen == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
        seen = rejected_entries(cx, window, view);
    }
    panic!("{seen} rejected entries, wanted {want}");
}

#[gpui_kit::test]
fn a_refusal_has_a_dismiss_control_that_removes_the_line_and_the_entry(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    state.session_status.store(400, Ordering::SeqCst);
    state.delete_status.store(418, Ordering::SeqCst);
    let (window, view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-plan");
    cx.update_window(window, |_, window, cx| window.click("thread-mode-plan", cx))
        .expect("window");
    wait_for_shown(cx, window, "thread-refused-0");
    wait_for_shown(cx, window, "thread-refused-dismiss-0");
    assert_eq!(rejected_entries(cx, window, &view), 1);
    // An unmapped code falls back to the code itself.
    cx.update_window(window, |_, window, cx| {
        window.click("queued-remove-tk_q", cx)
    })
    .expect("window");
    wait_for_shown(cx, window, "thread-refused-1");
    let text = label_of(cx, window, "thread-refused-1").expect("label");
    assert!(text.contains("teapot"), "{text}");
    assert_eq!(rejected_entries(cx, window, &view), 2);

    // Dismissing the first line drops it and its outbox entry; the other stays.
    cx.update_window(window, |_, window, cx| {
        window.click("thread-refused-dismiss-0", cx)
    })
    .expect("window");
    wait_for_gone(cx, window, "thread-refused-1");
    wait_for_rejected(cx, window, &view, 1);
    wait_for_shown(cx, window, "thread-refused-0");
    let text = label_of(cx, window, "thread-refused-0").expect("label");
    assert!(text.contains("remove"), "{text}");
    cx.update_window(window, |_, window, cx| {
        window.click("thread-refused-dismiss-0", cx)
    })
    .expect("window");
    wait_for_gone(cx, window, "thread-refused-0");
    wait_for_rejected(cx, window, &view, 0);
}

#[gpui_kit::test]
fn refusals_clear_when_the_thread_is_switched(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    state.session_status.store(400, Ordering::SeqCst);
    let (window, view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "thread-mode-plan");
    cx.update_window(window, |_, window, cx| window.click("thread-mode-plan", cx))
        .expect("window");
    wait_for_shown(cx, window, "thread-refused-0");
    assert_eq!(rejected_entries(cx, window, &view), 1);
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| {
            shell.open_thread("th_b", Some("/work/other"), cx)
        });
    })
    .expect("window");
    wait_for_gone(cx, window, "thread-refused-0");
    wait_for_rejected(cx, window, &view, 0);
    // Back on the first thread nothing resurfaces.
    open_thread_a(cx, window, &view);
    wait_for_shown(cx, window, "thread-mode-plan");
    assert!(
        cx.update_window(window, |_, window, _| window
            .try_find("thread-refused-0")
            .is_none())
            .expect("window")
    );
}

#[gpui_kit::test]
fn send_waits_for_the_threads_first_snapshot_and_keeps_the_text(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    open_thread_a(cx, window, &view);
    // Nothing has run since the thread was opened: no snapshot has reached the composer.
    cx.update_window(window, |_, window, cx| {
        let composer = view.read(cx).thread_composer().clone();
        assert!(!composer.read(cx).is_ready());
        let input = composer.read(cx).prompt_input().clone();
        input.update(cx, |state, cx| state.set_value("early words", window, cx));
        composer.update(cx, |composer, cx| composer.send(window, cx));
        assert_eq!(input.read(cx).value().as_ref(), "early words");
    })
    .expect("window");
    assert!(writes_to(&state, "POST", "/prompts").is_empty());
    // Once the snapshot is in, the text is still there and Send delivers it.
    wait_for_shown(cx, window, "thread-mode-plan");
    cx.update_window(window, |_, window, cx| {
        let composer = view.read(cx).thread_composer().clone();
        assert!(composer.read(cx).is_ready());
        let input = composer.read(cx).prompt_input().clone();
        assert_eq!(input.read(cx).value().as_ref(), "early words");
        window.click("thread-send", cx);
    })
    .expect("window");
    wait_until_true("the prompt to reach the hub", || {
        !writes_to(&state, "POST", "/prompts").is_empty()
    });
    assert_eq!(
        writes_to(&state, "POST", "/prompts")[0].body["parts"][0]["text"],
        "early words"
    );
}

#[gpui_kit::test]
fn a_started_removal_is_dropped_from_the_outbox_once_its_task_finished(cx: &mut TestAppContext) {
    let (state, hub) = hub_with_a_session(false);
    state.delete_status.store(409, Ordering::SeqCst);
    let (window, view) = open_composer_shell(cx, &hub);
    wait_for_shown(cx, window, "queued-remove-tk_q");
    cx.update_window(window, |_, window, cx| {
        window.click("queued-remove-tk_q", cx)
    })
    .expect("window");
    wait_for_shown(cx, window, "thread-started-notice");
    let lingering = |cx: &mut TestAppContext| {
        cx.update_window(window, |_, _, cx| {
            view.read(cx)
                .link()
                .expect("link")
                .with_thread(|access| access.outbox.started_removals().len())
        })
        .expect("window")
        .recv()
        .expect("outbox")
    };
    assert_eq!(lingering(cx), 1);
    // The task starts and finishes; the view shows it finished.
    push_live(
        &state,
        "th_a",
        task_event_frame(
            4,
            "tk_q",
            json!({ "type": "RunStarted", "reason": "prompt", "agent_id": "agt_claude",
                    "machine_id": "mch_laptop" }),
        ),
    );
    push_live(
        &state,
        "th_a",
        task_event_frame(
            5,
            "tk_q",
            json!({ "type": "RunFinished",
                    "outcome": { "type": "success", "stop_reason": "end_turn" } }),
        ),
    );
    wait_for_gone(cx, window, "thread-started-notice");
    let mut left = lingering(cx);
    for _ in 0..400 {
        if left == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(25));
        left = lingering(cx);
    }
    assert_eq!(left, 0, "the conflict lingers in the outbox");
}

// ---- notifications, hub links, notification settings --------------------------------------------

/// Fetch the inbox list again and wait for it to land (the refresh runs on a worker).
fn refresh_inbox(
    cx: &mut TestAppContext,
    window: gpui_kit::AnyWindowHandle,
    view: &Entity<AppShell>,
) {
    let applied =
        |cx: &mut TestAppContext| cx.update(|cx| view.read(cx).inbox().read(cx).lists_applied());
    let before = applied(cx);
    cx.update_window(window, |_, window, cx| {
        let inbox = view.read(cx).inbox().clone();
        inbox.update(cx, |inbox, cx| inbox.refresh(window, cx));
    })
    .expect("window");
    for _ in 0..400 {
        cx.executor().advance_clock(Duration::from_millis(50));
        cx.run_until_parked();
        if applied(cx) > before {
            return;
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    panic!("the inbox list never landed");
}

fn toasts(cx: &mut TestAppContext, view: &Entity<AppShell>) -> usize {
    cx.update(|cx| view.read(cx).toasts_shown())
}

fn set_status(state: &HubState, thread: &str, status: &str) {
    for row in state.threads.lock().expect("lock").iter_mut() {
        if row["thread_id"] == thread {
            row["status"] = json!(status);
        }
    }
}

fn notify_shell(
    cx: &mut TestAppContext,
    rows: Vec<Value>,
) -> (
    Arc<HubState>,
    String,
    gpui_kit::AnyWindowHandle,
    Entity<AppShell>,
) {
    let (state, hub) = hub_with(vec!["grant"], 600);
    state.socket_up.store(true, Ordering::SeqCst);
    *state.threads.lock().expect("lock") = rows;
    cx.executor().allow_parking();
    let (window, view) = open_shell_with(
        cx,
        &hub,
        stored_session_on(&hub),
        Arc::new(RecordingBrowser::new()),
        None,
        "",
        Arc::new(ManualClock::new(RAISED_MS)),
    );
    wait_for_inbox(cx, window, "inbox-open-th_a");
    (state, hub, window, view)
}

#[gpui_kit::test]
fn a_thread_moving_to_needs_you_notifies_once_and_the_first_fetch_is_only_a_baseline(
    cx: &mut TestAppContext,
) {
    let (state, _hub, window, view) = notify_shell(
        cx,
        vec![
            inbox_row("th_a", "Fix login", "running", "/work/repo"),
            inbox_row("th_b", "Already waiting", "needs_you", "/work/repo"),
        ],
    );
    assert_eq!(toasts(cx, &view), 0, "the seeded list shows no toast");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 0, "the same statuses show none");

    set_status(&state, "th_a", "needs_you");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 1);
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert_eq!(
            window.notifications(cx).len(),
            1,
            "the toast is in the window"
        );
    })
    .expect("window");

    refresh_inbox(cx, window, &view);
    assert_eq!(
        toasts(cx, &view),
        1,
        "a second refresh with the same status shows none"
    );
}

#[gpui_kit::test]
fn the_open_thread_does_not_notify_unless_the_window_is_in_the_background(cx: &mut TestAppContext) {
    let (state, _hub, window, view) = notify_shell(
        cx,
        vec![
            inbox_row("th_a", "Fix login", "running", "/work/repo"),
            inbox_row("th_b", "Other", "running", "/work/other"),
        ],
    );
    cx.update_window(window, |_, window, cx| {
        window.click("inbox-open-th_a", cx);
    })
    .expect("window");
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_a")));

    set_status(&state, "th_a", "needs_you");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 0, "the thread on screen stays quiet");

    // The window goes to the background: the open thread notifies like any other.
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.set_window_active(false, cx));
    })
    .expect("window");
    set_status(&state, "th_a", "done");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 1);

    // Back in front, a thread that is not the open one still notifies.
    cx.update_window(window, |_, _, cx| {
        view.update(cx, |shell, cx| shell.set_window_active(true, cx));
    })
    .expect("window");
    set_status(&state, "th_b", "needs_you");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 2);
}

#[gpui_kit::test]
fn a_failed_transition_notifies(cx: &mut TestAppContext) {
    let (state, _hub, window, view) = notify_shell(
        cx,
        vec![inbox_row("th_a", "Fix login", "running", "/work/repo")],
    );
    set_status(&state, "th_a", "failed");
    refresh_inbox(cx, window, &view);
    assert_eq!(toasts(cx, &view), 1);
}

#[gpui_kit::test]
fn a_click_on_a_notification_opens_its_thread(cx: &mut TestAppContext) {
    let (state, _hub, window, view) = notify_shell(
        cx,
        vec![
            inbox_row("th_a", "Fix login", "running", "/work/repo"),
            inbox_row("th_b", "Other", "running", "/work/other"),
        ],
    );
    assert!(state.subscribed.lock().expect("lock").is_empty());
    let events = cx.update(|cx| view.read(cx).notify_events());
    events.push(client_gpui::notify::NotifyEvent::Open {
        thread_id: "th_b".to_owned(),
    });
    cx.update_window(window, |_, window, cx| {
        view.update(cx, |shell, cx| shell.pump_notifications(window, cx));
    })
    .expect("window");
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_b")));
    wait_until_true("a subscription to th_b", || {
        *state.subscribed.lock().expect("lock") == ["th_b"]
    });
}

#[gpui_kit::test]
fn a_hub_link_opens_its_thread_and_other_urls_are_ignored(cx: &mut TestAppContext) {
    let (state, hub, window, view) = notify_shell(
        cx,
        vec![inbox_row("th_a", "Fix login", "running", "/work/repo")],
    );
    let open = |cx: &mut TestAppContext, url: &str| {
        let url = url.to_owned();
        cx.update_window(window, |_, _, cx| {
            view.update(cx, |shell, cx| shell.open_url(&url, cx))
        })
        .expect("window")
    };
    for url in [
        "https://evil.test/t/th_b".to_owned(),
        format!("{hub}/threads/th_b"),
        format!("{hub}/t/th_b/extra"),
        "not a url".to_owned(),
    ] {
        assert!(!open(cx, &url), "{url}");
    }
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), None));
    assert!(state.subscribed.lock().expect("lock").is_empty());

    assert!(open(cx, &format!("{hub}/t/th_b")));
    cx.update(|cx| assert_eq!(view.read(cx).open_thread_id(), Some("th_b")));
    wait_until_true("a subscription to th_b", || {
        *state.subscribed.lock().expect("lock") == ["th_b"]
    });
}

#[gpui_kit::test]
fn the_notification_settings_are_on_the_settings_page(cx: &mut TestAppContext) {
    let (_state, _hub, window, view) = notify_shell(
        cx,
        vec![inbox_row("th_a", "Fix login", "running", "/work/repo")],
    );
    cx.update_window(window, |_, window, cx| {
        window.render_frame(cx);
        assert!(window.try_find("notify-settings").is_none());
        view.update(cx, |shell, cx| shell.select(Section::Settings, cx));
        window.render_frame(cx);
        assert_eq!(
            window.find("notify-settings").label(),
            Some("Notification settings")
        );
        assert!(window.try_find("notify-row-needs_you").is_some());
        assert!(window.try_find("notify-row-failed").is_some());
    })
    .expect("window");
}
