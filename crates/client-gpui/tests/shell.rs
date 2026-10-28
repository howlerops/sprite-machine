//! M3-002: desktop shell. Sign-in by device-code pairing (QR) and GitHub OAuth PKCE against a
//! local fake hub, navigation, settings and theming, Fluent strings, accessibility labels and
//! focus order. State machines are tested directly; the view is tested on GPUI's headless platform.

use std::collections::HashSet;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::http::StatusCode;
use axum::routing::post;
use axum::{Json, Router};
use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use client_core::Connectivity;
use client_gpui::shell::auth::{LoginFailure, SessionGrant, api_url};
use client_gpui::shell::browser::{Browser, RecordingBrowser};
use client_gpui::shell::i18n::{self, Catalog};
use client_gpui::shell::login::{LoginEvent, LoginMethod, LoginModel, LoginStage};
use client_gpui::shell::model::{Screen, ShellModel};
use client_gpui::shell::nav::{NavModel, Section};
use client_gpui::shell::pairing::{self, DeviceCode, PollOutcome, PollSchedule, QrMatrix};
use client_gpui::shell::pkce::{self, Callback, GithubSignIn, LoopbackListener, Pkce};
use client_gpui::shell::prefs::{Appearance, Preferences, ThemeChoice};
use client_gpui::shell::session::{FileStore, MemoryStore, Session, SessionStore};
use client_gpui::shell::{AppConfig, AppShell};
use gpui_kit::component::ActiveTheme as _;
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
                ("uri", "X"),
                ("minutes", "5"),
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
    let opened = loop {
        if let Some(url) = browser.opened().first() {
            break url.clone();
        }
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
    with_settings.extend(["theme-system", "theme-light", "theme-dark", "sign-out"]);
    check(&shell, &with_settings);
}

// ---- the view, headless -------------------------------------------------------------------------

fn open_shell(
    cx: &mut TestAppContext,
    hub: &str,
    store: Arc<dyn SessionStore>,
    browser: Arc<dyn Browser>,
) -> (gpui_kit::AnyWindowHandle, Entity<AppShell>) {
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
                            prefs_path: None,
                            poll_speedup: 200,
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
fn api_urls_have_exactly_one_slash() {
    assert_eq!(api_url("https://h/", "/auth/x"), "https://h/v1/auth/x");
    assert_eq!(api_url("https://h", "auth/x"), "https://h/v1/auth/x");
}
