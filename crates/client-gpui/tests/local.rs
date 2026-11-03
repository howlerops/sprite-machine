//! M2B-015: the desktop embeds the runner. Plain-Rust tests of the connect-to-phone model and
//! the persisted setting, the Fluent catalog, and an integration test that starts the embedded
//! runner against a fake hub.
//!
//! The hub is a small in-process axum server that speaks the tunnel (hello/welcome, acks,
//! `thread_open`, `event`, `command_result`) and lists the threads it was told about on
//! `GET /v1/threads`, which is what a phone reads. A real hub (workerd, `scripts/hub-test.sh`)
//! is too heavy for `cargo test -p client-gpui`; the tunnel contract it plays is the one in
//! docs/specs/tunnel.md, and `crates/runner/tests/server_mode.rs` uses the same kind of fake.
//! The harness is the stub `opencode` of that test: a script on a private `PATH` that re-runs
//! this test binary as acp-mock (`mock_agent_child`).
#![cfg(unix)]

use std::collections::BTreeSet;
use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use axum::extract::State;
use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::response::IntoResponse;
use axum::routing::get;
use axum::{Json, Router};
use client_gpui::local::{
    Action, EN_FTL, LinkState, LocalConfig, LocalRunner, PhoneLink, RunnerStatus,
};
use client_gpui::shell::i18n::Catalog;
use client_gpui::shell::model::ShellModel;
use client_gpui::shell::prefs::Preferences;
use client_gpui::shell::session::MemoryStore;
use runner::embed::Config;
use runner::login::{CredentialStore, Login};
use runner::server::PermissionMode;
use runner::tunnel::Timing;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::sync::mpsc;

// ---- the model -----------------------------------------------------------------------------

#[test]
fn a_session_connects_and_disconnects_with_its_switch() {
    let mut link = PhoneLink::new(false);
    assert!(link.add_session("th_1", "one").is_empty());
    assert_eq!(link.row("th_1").unwrap().state, LinkState::Off);

    assert_eq!(link.toggle("th_1"), Some(Action::Connect("th_1".into())));
    assert_eq!(link.row("th_1").unwrap().state, LinkState::Connecting);
    assert_eq!(link.connected("th_1", "https://hub/t/th_1"), None);
    let row = link.row("th_1").unwrap();
    assert_eq!(row.state, LinkState::On);
    assert_eq!(row.url.as_deref(), Some("https://hub/t/th_1"));

    assert_eq!(link.toggle("th_1"), Some(Action::Disconnect("th_1".into())));
    let row = link.row("th_1").unwrap();
    assert_eq!(row.state, LinkState::Off);
    assert_eq!(row.url, None);
    assert_eq!(link.toggle("th_unknown"), None);
}

#[test]
fn auto_connect_connects_new_sessions_and_existing_off_ones() {
    let mut link = PhoneLink::new(true);
    assert_eq!(
        link.add_session("th_1", "one"),
        vec![Action::Connect("th_1".into())]
    );
    let mut link = PhoneLink::new(false);
    link.add_session("th_1", "one");
    link.add_session("th_2", "two");
    link.toggle("th_2");
    link.connected("th_2", "u");
    // Turning it on connects only the session that is off.
    assert_eq!(link.set_auto(true), vec![Action::Connect("th_1".into())]);
    // Turning it off disconnects nothing.
    assert!(link.set_auto(false).is_empty());
    assert_eq!(link.row("th_2").unwrap().state, LinkState::On);
    assert!(!link.auto());
}

#[test]
fn a_connect_that_finishes_after_the_switch_went_off_is_undone() {
    let mut link = PhoneLink::new(false);
    link.add_session("th_1", "one");
    link.toggle("th_1");
    link.toggle("th_1");
    assert_eq!(
        link.connected("th_1", "u"),
        Some(Action::Disconnect("th_1".into()))
    );
    assert_eq!(link.row("th_1").unwrap().state, LinkState::Off);
    // A failure after the switch went off is not shown either.
    link.toggle("th_1");
    link.toggle("th_1");
    link.failed("th_1", "late");
    assert_eq!(link.row("th_1").unwrap().state, LinkState::Off);
}

#[test]
fn a_failed_connect_is_shown_and_the_switch_tries_again() {
    let mut link = PhoneLink::new(false);
    link.add_session("th_1", "one");
    link.toggle("th_1");
    link.failed("th_1", "no hub");
    assert_eq!(
        link.row("th_1").unwrap().state,
        LinkState::Failed("no hub".into())
    );
    assert_eq!(link.toggle("th_1"), Some(Action::Connect("th_1".into())));
}

#[test]
fn the_global_setting_is_persisted_with_the_preferences() {
    let dir = TempDir::new().unwrap();
    let path = dir.path().join("preferences.json");
    assert!(!Preferences::load(&path).connect_to_phone_auto);
    let mut model = ShellModel::new(
        Arc::new(MemoryStore::default()),
        Preferences::default(),
        Some(path.clone()),
        "http://127.0.0.1:8787",
    );
    model.set_connect_to_phone_auto(true).unwrap();
    assert!(Preferences::load(&path).connect_to_phone_auto);
    model.set_connect_to_phone_auto(false).unwrap();
    assert!(!Preferences::load(&path).connect_to_phone_auto);
}

// ---- the catalog ---------------------------------------------------------------------------

#[test]
fn every_phone_string_the_code_uses_is_in_the_catalog_and_none_is_dead() {
    let catalog = Catalog::from_source("en", EN_FTL).expect("local.ftl parses");
    let sources = [
        include_str!("../src/local/model.rs"),
        include_str!("../src/local/runner.rs"),
        include_str!("../src/local/view.rs"),
    ];
    let mut used = BTreeSet::new();
    for source in sources {
        for part in source.split('"').skip(1).step_by(2) {
            if part.starts_with("phone-")
                && part
                    .chars()
                    .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
            {
                used.insert(part.to_owned());
            }
        }
    }
    let ids: BTreeSet<String> = Catalog::ids(EN_FTL).into_iter().collect();
    let missing: Vec<_> = used.iter().filter(|id| !catalog.has(id)).collect();
    assert!(
        missing.is_empty(),
        "ids missing from local.ftl: {missing:?}"
    );
    let unused: Vec<_> = ids.iter().filter(|id| !used.contains(*id)).collect();
    assert!(unused.is_empty(), "dead ids in local.ftl: {unused:?}");
    assert_eq!(
        catalog.tr("phone-state-failed", &[("reason", "no hub")]),
        "Could not connect: no hub"
    );
}

// ---- the fake hub --------------------------------------------------------------------------

#[derive(Default)]
struct HubState {
    /// `(thread_id, title)` of each `thread_open`.
    threads: Vec<(String, String)>,
    /// `(thread_id, event type)` of each `event`.
    events: Vec<(String, String)>,
    /// The `command_result` frames.
    results: Vec<Value>,
    /// Down frames for the connected runner.
    down: Option<mpsc::UnboundedSender<String>>,
    hellos: usize,
}

type Hub = Arc<Mutex<HubState>>;

async fn start_hub() -> (Hub, String) {
    let hub: Hub = Hub::default();
    let app = Router::new()
        .route("/v1/machines/{id}/tunnel", get(tunnel))
        .route("/v1/threads", get(threads))
        .with_state(hub.clone());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    tokio::spawn(async move {
        axum::serve(listener, app).await.unwrap();
    });
    (hub, format!("ws://{addr}"))
}

async fn tunnel(State(hub): State<Hub>, upgrade: WebSocketUpgrade) -> impl IntoResponse {
    upgrade
        .protocols(["sm.tunnel.v1"])
        .on_upgrade(move |socket| serve(hub, socket))
}

async fn serve(hub: Hub, mut socket: WebSocket) {
    let (down_tx, mut down_rx) = mpsc::unbounded_channel::<String>();
    hub.lock().unwrap().down = Some(down_tx);
    loop {
        tokio::select! {
            frame = socket.recv() => {
                let Some(Ok(Message::Text(text))) = frame else { return };
                let Ok(frame) = serde_json::from_str::<Value>(text.as_str()) else { continue };
                let seq = frame["seq"].as_u64();
                {
                    let mut state = hub.lock().unwrap();
                    match frame["t"].as_str() {
                        Some("thread_open") => state.threads.push((
                            frame["thread_id"].as_str().unwrap().to_owned(),
                            frame["title"].as_str().unwrap().to_owned(),
                        )),
                        Some("event") => state.events.push((
                            frame["envelope"]["thread_id"].as_str().unwrap().to_owned(),
                            frame["envelope"]["event"]["type"].as_str().unwrap().to_owned(),
                        )),
                        Some("command_result") => state.results.push(frame.clone()),
                        Some("hello") => state.hellos += 1,
                        _ => {}
                    }
                }
                let reply = match frame["t"].as_str() {
                    Some("hello") => Some(json!({
                        "t": "welcome", "proto": 1, "resumed": false, "resume_token": "tok",
                        "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
                    })),
                    _ => seq.map(|seq| json!({"t": "ack", "seq": seq})),
                };
                if let Some(reply) = reply
                    && socket.send(Message::Text(reply.to_string().into())).await.is_err()
                {
                    return;
                }
            }
            Some(down) = down_rx.recv() => {
                if socket.send(Message::Text(down.into())).await.is_err() {
                    return;
                }
            }
        }
    }
}

/// What a phone reads: the threads the hub was told about.
async fn threads(State(hub): State<Hub>) -> Json<Value> {
    let state = hub.lock().unwrap();
    Json(json!({
        "threads": state.threads.iter()
            .map(|(id, title)| json!({"thread_id": id, "title": title}))
            .collect::<Vec<_>>()
    }))
}

async fn phone_sees(base: &str) -> Vec<String> {
    let url = format!("http://{}/v1/threads", base.trim_start_matches("ws://"));
    let body: Value = reqwest::get(url).await.unwrap().json().await.unwrap();
    body["threads"]
        .as_array()
        .unwrap()
        .iter()
        .map(|t| t["thread_id"].as_str().unwrap().to_owned())
        .collect()
}

fn send_prompt(hub: &Hub, seq: u64, thread: &str, text: &str) {
    let frame = json!({
        "t": "command", "seq": seq, "idempotency_key": format!("idk_{seq}"),
        "issued_at": "2026-11-03T10:00:00.000Z",
        "command": {
            "kind": "prompt", "thread_id": thread, "task_id": format!("tk_phone{seq}"),
            "message_id": format!("msg_phone{seq}"),
            "parts": [{"type": "text", "text": text}],
            "author": {"kind": "user", "id": "usr_1", "device_id": "dev_phone"},
        },
    });
    hub.lock()
        .unwrap()
        .down
        .as_ref()
        .expect("the runner is connected")
        .send(frame.to_string())
        .unwrap();
}

async fn eventually(what: &str, mut check: impl FnMut() -> bool) {
    for _ in 0..400 {
        if check() {
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("timed out waiting for {what}");
}

// ---- the agent -----------------------------------------------------------------------------

const MOCK_DIR_ENV: &str = "SM_DESKTOP_LOCAL_MOCK_DIR";

/// The agent process; it runs only when the stub harness re-runs this binary with
/// `MOCK_DIR_ENV`. Every agent says "hello from the agent" to its first prompt.
#[tokio::test]
#[ignore = "run by the stub harness of the local tests"]
async fn mock_agent_child() {
    let Ok(dir) = std::env::var(MOCK_DIR_ENV) else {
        return;
    };
    let script = fs::read_to_string(PathBuf::from(dir).join("script.json")).unwrap();
    let script = acp_mock::Script::from_json(&script).expect("valid script");
    println!();
    acp_mock::run_recorded(
        script,
        acp_mock::Recorder::new(),
        tokio::io::stdin(),
        tokio::io::stdout(),
    )
    .await
    .expect("agent serves until stdin closes");
}

// ---- the rig -------------------------------------------------------------------------------

struct Rig {
    _dir: TempDir,
    root: PathBuf,
    hub: Hub,
    base: String,
}

impl Rig {
    async fn new() -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["state", "home", "bin", "work", "mock"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        fs::write(
            root.join("mock/script.json"),
            json!({ "turns": [{ "steps": [{ "text": "hello from the agent" }] }] }).to_string(),
        )
        .unwrap();
        let stub = root.join("bin/opencode");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\n{MOCK_DIR_ENV}='{}' exec '{}' --ignored --exact mock_agent_child --nocapture --test-threads=1 2>/dev/null\n",
                root.join("mock").display(),
                std::env::current_exe().unwrap().display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let (hub, base) = start_hub().await;
        let rig = Self {
            _dir: dir,
            root,
            hub,
            base,
        };
        CredentialStore::file_only(rig.root.join("credential.json"))
            .save(&Login {
                hub: rig.base.clone(),
                machine_id: "mch_desktop".to_owned(),
                credential: "smm_test_credential".to_owned(),
            })
            .unwrap();
        rig
    }

    fn runner(&self, auto: bool) -> LocalRunner {
        let root = self.root.clone();
        LocalRunner::new(
            LocalConfig::new(move || {
                Ok(Config {
                    credentials: CredentialStore::file_only(root.join("credential.json")),
                    state_dir: root.join("state"),
                    home: root.join("home"),
                    path_env: root.join("bin").into(),
                    permission_mode: PermissionMode::Default,
                    timing: Timing {
                        backoff_base: Duration::from_millis(20),
                        backoff_cap: Duration::from_millis(200),
                        stable_after: Duration::from_secs(3600),
                        welcome_timeout: Duration::from_secs(5),
                    },
                })
            }),
            auto,
        )
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn event_count(&self, thread: &str) -> usize {
        self.hub
            .lock()
            .unwrap()
            .events
            .iter()
            .filter(|(t, _)| t == thread)
            .count()
    }
}

// ---- the embedded runner against the hub ----------------------------------------------------

#[tokio::test]
async fn a_session_is_visible_to_the_phone_only_while_it_is_connected() {
    let rig = Rig::new().await;
    let runner = rig.runner(false);
    runner.trust(&rig.work()).await.unwrap();
    assert_eq!(runner.snapshot().status, RunnerStatus::Running);
    eventually("the tunnel hello", || rig.hub.lock().unwrap().hellos == 1).await;

    let one = runner
        .open_session(rig.work(), Some("one".into()), None)
        .await
        .unwrap();
    let two = runner
        .open_session(rig.work(), Some("two".into()), None)
        .await
        .unwrap();
    // Running locally is not enough: the hub (and so the phone) knows neither.
    assert!(phone_sees(&rig.base).await.is_empty());

    // Connect: the session appears, and the phone can prompt it.
    runner.toggle(&one).await;
    eventually("the thread on the hub", || {
        rig.hub
            .lock()
            .unwrap()
            .threads
            .iter()
            .any(|(t, _)| *t == one)
    })
    .await;
    assert_eq!(phone_sees(&rig.base).await, vec![one.clone()]);
    let snapshot = runner.snapshot();
    let row = snapshot.rows.iter().find(|r| r.thread_id == one).unwrap();
    assert_eq!(row.state, LinkState::On);
    assert!(row.url.as_deref().unwrap().ends_with(&format!("/t/{one}")));

    send_prompt(&rig.hub, 1, &one, "hi");
    eventually("the agent's events on the hub", || {
        rig.hub
            .lock()
            .unwrap()
            .events
            .iter()
            .any(|(t, e)| *t == one && e == "RunFinished")
    })
    .await;

    // Disconnect: nothing new reaches the hub, and the phone's prompt is refused.
    runner.toggle(&one).await;
    assert_eq!(
        runner
            .snapshot()
            .rows
            .iter()
            .find(|r| r.thread_id == one)
            .unwrap()
            .state,
        LinkState::Off
    );
    let before = rig.event_count(&one);
    send_prompt(&rig.hub, 2, &one, "again");
    eventually("the refusal", || {
        rig.hub
            .lock()
            .unwrap()
            .results
            .iter()
            .any(|r| r["ok"] == false && r["code"] == "unknown_thread")
    })
    .await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert_eq!(rig.event_count(&one), before, "events after disconnect");
    // The other session was never connected and is still unknown to the hub.
    assert_eq!(phone_sees(&rig.base).await, vec![one.clone()]);
    assert!(!phone_sees(&rig.base).await.contains(&two));
    runner.stop().await;
    assert_eq!(runner.snapshot().status, RunnerStatus::Stopped);
}

#[tokio::test]
async fn auto_connect_shares_new_and_existing_sessions_without_a_toggle() {
    let rig = Rig::new().await;
    let runner = rig.runner(false);
    runner.trust(&rig.work()).await.unwrap();
    let early = runner
        .open_session(rig.work(), Some("early".into()), None)
        .await
        .unwrap();
    assert!(phone_sees(&rig.base).await.is_empty());

    runner.set_auto_connect(true).await;
    let late = runner
        .open_session(rig.work(), Some("late".into()), None)
        .await
        .unwrap();
    eventually("both threads on the hub", || {
        rig.hub.lock().unwrap().threads.len() == 2
    })
    .await;
    let seen = phone_sees(&rig.base).await;
    assert!(seen.contains(&early) && seen.contains(&late));
    assert!(runner.snapshot().auto);
    runner.stop().await;
}

#[tokio::test]
async fn a_computer_that_was_never_enrolled_says_so_and_connects_nothing() {
    let rig = Rig::new().await;
    // No credential file: `sm login` was never run.
    fs::remove_file(rig.root.join("credential.json")).unwrap();
    let runner = rig.runner(false);
    assert!(runner.start().await.is_err());
    assert_eq!(runner.snapshot().status, RunnerStatus::NotEnrolled);
    assert_eq!(rig.hub.lock().unwrap().hellos, 0);
}
