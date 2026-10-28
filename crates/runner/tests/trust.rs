//! Workspace trust and the remote-terminal opt-in (M2B-008, PLAN-v2-ade.md §4.10). The store, the
//! question and the audit log are tested directly; the dispatch tests run `sm rc` against an
//! in-process fake hub, with a stub harness that records that an agent started.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::{PermissionsExt, symlink};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use runner::login::{CredentialStore, Login};
use runner::server::{self, Options, PermissionMode};
use runner::trust::{
    Asker, Audit, TrustStore, Verdict, ask_on, decide, grant, remote_terminal_allowed, revoke,
};
use runner::tunnel::Timing;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::oneshot;
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::header::SEC_WEBSOCKET_PROTOCOL;

const WAIT: Duration = Duration::from_secs(20);

/// A fresh directory, canonical, so the tests compare canonical paths.
fn tempdir() -> (TempDir, PathBuf) {
    let dir = TempDir::new().unwrap();
    let root = dir.path().canonicalize().unwrap();
    (dir, root)
}

fn audit_events(state: &Path) -> Vec<Value> {
    fs::read_to_string(state.join("audit.jsonl"))
        .unwrap_or_default()
        .lines()
        .map(|line| serde_json::from_str(line).unwrap())
        .collect()
}

fn counting_asker(answer: bool) -> (Asker, Arc<Mutex<u32>>) {
    let asked = Arc::new(Mutex::new(0));
    let counter = Arc::clone(&asked);
    let asker: Asker = Arc::new(move |_| {
        *counter.lock().unwrap() += 1;
        answer
    });
    (asker, asked)
}

// ---- the store and the decision -----------------------------------------------------------------

#[test]
fn the_first_use_asks_and_a_yes_is_stored_so_the_next_use_is_not_asked() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = TrustStore::new(&state, &root.join("home"));
    let audit = Audit::new(&state);
    let (yes, asked) = counting_asker(true);

    assert_eq!(
        decide(&store, &audit, &project, Some(&yes)).unwrap(),
        Verdict::Granted
    );
    assert_eq!(
        decide(&store, &audit, &project, Some(&yes)).unwrap(),
        Verdict::Trusted
    );
    assert_eq!(*asked.lock().unwrap(), 1, "the second use must not ask");

    let mode = fs::metadata(state.join("trust.json"))
        .unwrap()
        .permissions()
        .mode();
    assert_eq!(
        mode & 0o777,
        0o600,
        "trust.json must be readable by its owner only"
    );
    assert_eq!(store.list().unwrap(), vec![project.canonicalize().unwrap()]);
    let events = audit_events(&state);
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"], "trust_granted");
    assert_eq!(events[0]["by"], "prompt");
}

#[test]
fn a_no_and_a_missing_terminal_both_refuse_and_store_nothing() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let store = TrustStore::new(&state, &root.join("home"));
    let audit = Audit::new(&state);

    let (no, asked) = counting_asker(false);
    assert_eq!(
        decide(&store, &audit, &project, Some(&no)).unwrap(),
        Verdict::Refused
    );
    assert_eq!(*asked.lock().unwrap(), 1);
    assert!(!store.is_trusted(&project).unwrap());
    assert_eq!(audit_events(&state)[0]["event"], "trust_declined");

    // Without a terminal nothing is asked, and the directory is refused.
    assert_eq!(
        decide(&store, &audit, &project, None).unwrap(),
        Verdict::Refused
    );
    assert!(!store.is_trusted(&project).unwrap());
    assert!(!state.join("trust.json").exists());
}

#[test]
fn the_home_directory_and_root_are_asked_every_time_and_never_stored() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let store = TrustStore::new(&state, &home);
    let audit = Audit::new(&state);
    let (yes, asked) = counting_asker(true);

    assert_eq!(
        decide(&store, &audit, &home, Some(&yes)).unwrap(),
        Verdict::Once
    );
    assert_eq!(
        decide(&store, &audit, &home, Some(&yes)).unwrap(),
        Verdict::Once
    );
    assert_eq!(*asked.lock().unwrap(), 2, "home is asked on every use");
    assert_eq!(
        decide(&store, &audit, Path::new("/"), Some(&yes)).unwrap(),
        Verdict::Once
    );
    assert!(!state.join("trust.json").exists(), "nothing may be stored");
    assert!(store.trust(&home).is_err());
    assert!(store.trust(Path::new("/")).is_err());
    assert_eq!(
        decide(&store, &audit, &home, None).unwrap(),
        Verdict::Refused
    );

    // A hand-written entry for home does not make it trusted either.
    fs::write(
        state.join("trust.json"),
        json!({ "trusted": [home, "/"] }).to_string(),
    )
    .unwrap();
    assert!(!store.is_trusted(&home).unwrap());
    assert!(!store.is_trusted(Path::new("/")).unwrap());
}

#[test]
fn trust_is_by_canonical_path_so_a_symlink_resolves_to_its_target() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let real = root.join("real");
    fs::create_dir_all(&real).unwrap();
    let link = root.join("link");
    symlink(&real, &link).unwrap();
    let store = TrustStore::new(&state, &root.join("home"));
    let audit = Audit::new(&state);

    assert_eq!(store.trust(&link).unwrap(), real);
    assert!(store.is_trusted(&real).unwrap(), "stored by its target");
    assert!(
        store.is_trusted(&link).unwrap(),
        "looked up through the link"
    );
    let (no, _) = counting_asker(false);
    assert_eq!(
        decide(&store, &audit, &link, Some(&no)).unwrap(),
        Verdict::Trusted,
        "the decision looks up the canonical path"
    );
    assert_eq!(store.list().unwrap(), vec![real.clone()]);
    assert!(store.untrust(&link).unwrap());
    assert!(!store.is_trusted(&real).unwrap());
}

#[test]
fn a_trust_file_that_does_not_parse_trusts_nothing_and_is_an_error() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    fs::write(state.join("trust.json"), "not json").unwrap();
    let store = TrustStore::new(&state, &root.join("home"));
    assert!(store.is_trusted(&project).is_err());
}

#[test]
fn sm_trust_and_sm_untrust_manage_the_store_and_each_leave_an_audit_line() {
    let (_guard, root) = tempdir();
    let state = root.join("state");
    fs::create_dir_all(&state).unwrap();
    let project = root.join("project");
    fs::create_dir_all(&project).unwrap();
    let home = root.join("home");
    fs::create_dir_all(&home).unwrap();
    let store = TrustStore::new(&state, &home);
    let audit = Audit::new(&state);

    assert_eq!(
        grant(&store, &audit, &project, "sm trust").unwrap(),
        project.canonicalize().unwrap()
    );
    assert!(store.is_trusted(&project).unwrap());
    assert!(grant(&store, &audit, &home, "sm trust").is_err());
    assert!(revoke(&store, &audit, &project, "sm untrust").unwrap());
    assert!(!revoke(&store, &audit, &project, "sm untrust").unwrap());
    assert!(!store.is_trusted(&project).unwrap());

    let events = audit_events(&state);
    let granted: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"] == "trust_granted")
        .collect();
    assert_eq!(granted.len(), 1);
    assert_eq!(granted[0]["by"], "sm trust");
    let removed: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"] == "trust_removed")
        .collect();
    assert_eq!(
        removed.len(),
        1,
        "a second untrust removes nothing and is not logged"
    );
    assert_eq!(removed[0]["by"], "sm untrust");
    assert!(removed[0]["at"].as_str().unwrap().ends_with('Z'));
}

#[test]
fn the_question_is_trust_dir_and_only_yes_answers_yes() {
    let dir = Path::new("/work/app");
    for (input, expected) in [
        ("y\n", true),
        ("Yes\n", true),
        ("\n", false),
        ("n\n", false),
        ("maybe\n", false),
        ("", false),
    ] {
        let mut out = Vec::new();
        let answer = ask_on(dir, &mut input.as_bytes(), &mut out).unwrap();
        assert_eq!(answer, expected, "input {input:?}");
        assert_eq!(String::from_utf8(out).unwrap(), "Trust /work/app? [y/N] ");
    }
}

#[test]
fn the_remote_terminal_is_off_unless_the_flag_or_the_state_setting_turns_it_on() {
    let (_guard, state) = tempdir();
    assert!(!remote_terminal_allowed(&state, false));
    assert!(remote_terminal_allowed(&state, true));
    fs::write(
        state.join("settings.json"),
        r#"{"allow_remote_terminal": true}"#,
    )
    .unwrap();
    assert!(remote_terminal_allowed(&state, false));
    fs::write(
        state.join("settings.json"),
        r#"{"allow_remote_terminal": false}"#,
    )
    .unwrap();
    assert!(!remote_terminal_allowed(&state, false));
    fs::write(state.join("settings.json"), "not json").unwrap();
    assert!(!remote_terminal_allowed(&state, false));
}

// ---- dispatch from the hub ----------------------------------------------------------------------

/// Echoes the subprotocol the runner offers (its credential), as the hub does.
struct Echo;

impl Callback for Echo {
    fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
        if let Some(protocol) = req.headers().get(SEC_WEBSOCKET_PROTOCOL) {
            resp.headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, protocol.clone());
        }
        Ok(resp)
    }
}

struct Rig {
    _dir: TempDir,
    root: PathBuf,
    listener: TcpListener,
}

impl Rig {
    async fn new() -> Self {
        let (dir, root) = tempdir();
        for sub in ["state", "home", "bin", "work"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        // The stub harness: `opencode acp` is how discovery finds it. An agent that starts
        // leaves `started` behind and exits.
        let stub = root.join("bin/opencode");
        fs::write(
            &stub,
            format!(
                "#!/bin/sh\ntouch '{}'\nexit 1\n",
                root.join("started").display()
            ),
        )
        .unwrap();
        fs::set_permissions(&stub, fs::Permissions::from_mode(0o755)).unwrap();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let hub = format!("ws://{}", listener.local_addr().unwrap());
        CredentialStore::file_only(root.join("credential.json"))
            .save(&Login {
                hub,
                machine_id: "mch_test".to_owned(),
                credential: "smm_test_credential".to_owned(),
            })
            .unwrap();
        Self {
            _dir: dir,
            root,
            listener,
        }
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn started(&self) -> PathBuf {
        self.root.join("started")
    }

    fn options(&self, asker: Option<Asker>) -> Options {
        Options {
            credentials: CredentialStore::file_only(self.root.join("credential.json")),
            state_dir: self.state(),
            cwd: self.work(),
            home: self.root.join("home"),
            path_env: self.root.join("bin").into(),
            spawn: None,
            capacity: 32,
            permission_mode: PermissionMode::Default,
            name_prefix: "tst".to_owned(),
            continue_last: false,
            idle_timeout: Duration::from_secs(3600),
            timing: Timing {
                backoff_base: Duration::from_millis(20),
                backoff_cap: Duration::from_millis(200),
                stable_after: Duration::from_secs(3600),
                welcome_timeout: Duration::from_secs(5),
            },
            signin: None,
            asker,
            allow_remote_terminal: false,
        }
    }

    fn start(&self, asker: Option<Asker>) -> (oneshot::Sender<()>, JoinHandle<anyhow::Result<()>>) {
        let (stop, rx) = oneshot::channel::<()>();
        let task = tokio::spawn(server::run(self.options(asker), async {
            let _ = rx.await;
        }));
        (stop, task)
    }

    /// Accepts the runner, answers its `hello` with a fresh-stream `welcome`.
    async fn connect(&self) -> WebSocketStream<TcpStream> {
        let (stream, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("the runner did not connect")
            .unwrap();
        let mut ws = tokio_tungstenite::accept_hdr_async(stream, Echo)
            .await
            .unwrap();
        let hello = recv(&mut ws).await;
        assert_eq!(hello["t"], "hello");
        send(
            &mut ws,
            json!({
                "t": "welcome", "proto": 1, "resumed": false, "resume_token": "tok",
                "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
            }),
        )
        .await;
        ws
    }

    async fn stop(stop: oneshot::Sender<()>, task: JoinHandle<anyhow::Result<()>>) {
        let _ = stop.send(());
        tokio::time::timeout(WAIT, task)
            .await
            .expect("the server stopped")
            .unwrap()
            .expect("the server ended cleanly");
    }
}

async fn send(ws: &mut WebSocketStream<TcpStream>, frame: Value) {
    ws.send(Message::text(frame.to_string()))
        .await
        .expect("hub send");
}

async fn recv(ws: &mut WebSocketStream<TcpStream>) -> Value {
    loop {
        match tokio::time::timeout(WAIT, ws.next())
            .await
            .expect("the runner sent no frame")
        {
            Some(Ok(Message::Text(text))) => return serde_json::from_str(text.as_str()).unwrap(),
            Some(Ok(_)) => {}
            other => panic!("connection ended: {other:?}"),
        }
    }
}

/// Sends a dispatch of `path` on a fresh thread; returns its idempotency key.
async fn dispatch(ws: &mut WebSocketStream<TcpStream>, path: &Path) -> String {
    static KEYS: AtomicU64 = AtomicU64::new(0);
    let key = format!("idk_trust_{}", KEYS.fetch_add(1, Ordering::Relaxed));
    send(
        ws,
        json!({
            "t": "command", "seq": 1, "idempotency_key": key,
            "issued_at": "2026-10-27T22:01:00.000Z",
            "command": {
                "kind": "dispatch", "thread_id": "th_1", "task_id": "tk_1", "agent": "opencode",
                "workspace": { "path": path }, "permission_mode": "default",
                "message_id": "msg_1",
                "parts": [{ "type": "text", "text": "hi" }],
                "author": { "kind": "user", "id": "usr_1", "device_id": "dev_1" },
            },
        }),
    )
    .await;
    key
}

/// The `command_result` of `key`, skipping the other frames.
async fn command_result(ws: &mut WebSocketStream<TcpStream>, key: &str) -> Value {
    loop {
        let frame = recv(ws).await;
        if frame["t"] == "command_result" && frame["idempotency_key"] == key {
            return frame;
        }
    }
}

async fn wait_for_file(path: &Path) {
    let waited = tokio::time::timeout(WAIT, async {
        while !path.exists() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await;
    assert!(waited.is_ok(), "{} never appeared", path.display());
}

#[tokio::test]
async fn a_dispatch_into_an_untrusted_directory_fails_with_workspace_untrusted_and_starts_no_agent()
{
    let rig = Rig::new().await;
    let dir = rig.work().join("untrusted");
    fs::create_dir_all(&dir).unwrap();
    let (stop, task) = rig.start(None);
    let mut ws = rig.connect().await;

    let key = dispatch(&mut ws, &dir).await;
    let result = command_result(&mut ws, &key).await;
    assert_eq!(result["ok"], false);
    assert_eq!(result["code"], "workspace_untrusted");

    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(
        !rig.started().exists(),
        "an agent started in an untrusted directory"
    );
    let refused = audit_events(&rig.state())
        .into_iter()
        .find(|e| e["event"] == "dispatch_refused")
        .expect("the refusal is audited");
    assert_eq!(refused["code"], "workspace_untrusted");
    drop(ws);
    Rig::stop(stop, task).await;
}

#[tokio::test]
async fn a_dispatch_into_a_trusted_directory_starts_the_agent() {
    let rig = Rig::new().await;
    let dir = rig.work().join("trusted");
    fs::create_dir_all(&dir).unwrap();
    TrustStore::new(&rig.state(), &rig.root.join("home"))
        .trust(&dir)
        .unwrap();
    let (stop, task) = rig.start(None);
    let mut ws = rig.connect().await;

    dispatch(&mut ws, &dir).await;
    wait_for_file(&rig.started()).await;
    drop(ws);
    Rig::stop(stop, task).await;
}

#[tokio::test]
async fn answering_yes_at_the_terminal_trusts_the_directory_and_starts_the_session() {
    let rig = Rig::new().await;
    let dir = rig.work().join("asked");
    fs::create_dir_all(&dir).unwrap();
    let (yes, asked) = counting_asker(true);
    let (stop, task) = rig.start(Some(yes));
    let mut ws = rig.connect().await;

    dispatch(&mut ws, &dir).await;
    wait_for_file(&rig.started()).await;
    assert_eq!(*asked.lock().unwrap(), 1);
    assert!(
        TrustStore::new(&rig.state(), &rig.root.join("home"))
            .is_trusted(&dir)
            .unwrap(),
        "the yes is stored"
    );
    drop(ws);
    Rig::stop(stop, task).await;
}

#[tokio::test]
async fn answering_no_at_the_terminal_refuses_the_dispatch_and_starts_no_agent() {
    let rig = Rig::new().await;
    let dir = rig.work().join("declined");
    fs::create_dir_all(&dir).unwrap();
    let (no, asked) = counting_asker(false);
    let (stop, task) = rig.start(Some(no));
    let mut ws = rig.connect().await;

    let key = dispatch(&mut ws, &dir).await;
    let result = command_result(&mut ws, &key).await;
    assert_eq!(result["code"], "workspace_untrusted");
    assert_eq!(*asked.lock().unwrap(), 1);
    tokio::time::sleep(Duration::from_millis(500)).await;
    assert!(!rig.started().exists());
    assert!(
        !TrustStore::new(&rig.state(), &rig.root.join("home"))
            .is_trusted(&dir)
            .unwrap()
    );
    drop(ws);
    Rig::stop(stop, task).await;
}
