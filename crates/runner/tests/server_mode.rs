//! `sm rc` server mode (M2B-007) against an in-process fake hub, with acp-mock as the only
//! harness. The harness is a stub `opencode` on a fake PATH that re-runs this test binary as the
//! agent (see `mock_agent_child`), so the runner starts, kills and restarts real child processes.
#![cfg(unix)]

use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command as Process;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use futures_util::{SinkExt, StreamExt};
use runner::login::{CredentialStore, Login};
use runner::server::{self, Options, PermissionMode, Projects, RcArgs, SpawnMode};
use runner::trust::TrustStore;
use runner::tunnel::Timing;
use serde_json::{Value, json};
use tempfile::TempDir;
use tokio::io::AsyncWriteExt;
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
const MOCK_DIR_ENV: &str = "SM_SERVER_MODE_MOCK_DIR";

// ---- the agent -------------------------------------------------------------------------------

/// The agent process. It is not a test: it runs only when the stub harness re-runs this binary
/// with `MOCK_DIR_ENV`. It records its pid in `pids` and serves `script-<n>.json`, where `n` is
/// how many agents started so far (the last script that exists serves later ones).
#[tokio::test]
#[ignore = "run by the stub harness of the server_mode tests"]
async fn mock_agent_child() {
    let Ok(dir) = std::env::var(MOCK_DIR_ENV) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let pids = dir.join("pids");
    let mut log = fs::read_to_string(&pids).unwrap_or_default();
    log.push_str(&format!("{}\n", std::process::id()));
    fs::write(&pids, &log).expect("write pids");
    let n = log.lines().count();
    let script = (1..=n)
        .rev()
        .find_map(|i| fs::read_to_string(dir.join(format!("script-{i}.json"))).ok())
        .expect("a script");
    let script = acp_mock::Script::from_json(&script).expect("valid script");
    // The test harness printed `test ... ` without a newline: end that line.
    println!();
    // What the runner sent is kept in `recorded-<n>.json` (a dump every 20 ms).
    let recorder = acp_mock::Recorder::new();
    let dump = dir.join(format!("recorded-{n}.json"));
    let dumper = {
        let recorder = recorder.clone();
        tokio::spawn(async move {
            loop {
                let _ = fs::write(&dump, Value::Array(recorder.messages()).to_string());
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
        })
    };
    acp_mock::run_recorded(script, recorder, tokio::io::stdin(), tokio::io::stdout())
        .await
        .expect("agent serves until stdin closes");
    dumper.abort();
}

// ---- the fake hub ----------------------------------------------------------------------------

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

struct Hub {
    listener: TcpListener,
}

struct Conn {
    ws: WebSocketStream<TcpStream>,
    seq: u64,
}

impl Hub {
    async fn start() -> Self {
        Self {
            listener: TcpListener::bind("127.0.0.1:0").await.unwrap(),
        }
    }

    fn base(&self) -> String {
        format!("ws://{}", self.listener.local_addr().unwrap())
    }

    /// Accepts the runner, reads `hello` and answers a fresh-stream `welcome`.
    async fn accept(&self) -> (Conn, Value) {
        let (stream, _) = tokio::time::timeout(WAIT, self.listener.accept())
            .await
            .expect("the runner did not connect")
            .unwrap();
        // The runner offers the credential as a subprotocol and wants it echoed.
        let ws = tokio_tungstenite::accept_hdr_async(stream, Echo)
            .await
            .unwrap();
        let mut conn = Conn { ws, seq: 0 };
        let hello = conn.recv().await;
        assert_eq!(hello["t"], "hello");
        conn.send(json!({
            "t": "welcome", "proto": 1, "resumed": false, "resume_token": "tok",
            "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
        }))
        .await;
        (conn, hello)
    }
}

impl Conn {
    async fn send(&mut self, frame: Value) {
        self.ws
            .send(Message::text(frame.to_string()))
            .await
            .expect("hub send");
    }

    async fn recv(&mut self) -> Value {
        loop {
            match tokio::time::timeout(WAIT, self.ws.next())
                .await
                .expect("the runner sent no frame")
            {
                Some(Ok(Message::Text(text))) => {
                    return serde_json::from_str(text.as_str()).unwrap();
                }
                Some(Ok(_)) => {}
                other => panic!("connection ended: {other:?}"),
            }
        }
    }

    /// Frames up to and including the first one `matches`; panics with what it saw after 10 s.
    async fn until(&mut self, matches: impl Fn(&Value) -> bool) -> Vec<Value> {
        let mut seen = Vec::new();
        let waited = tokio::time::timeout(Duration::from_secs(10), async {
            loop {
                let frame = self.recv().await;
                let done = matches(&frame);
                seen.push(frame);
                if done {
                    return;
                }
            }
        })
        .await;
        assert!(waited.is_ok(), "no matching frame; saw {seen:#?}");
        seen
    }

    /// Sends a command with a fresh seq; returns its idempotency key.
    async fn command(&mut self, command: Value) -> String {
        self.seq += 1;
        // Keys are unique across runs: the runner remembers processed keys.
        static KEYS: AtomicU64 = AtomicU64::new(0);
        let key = format!("idk_{}", KEYS.fetch_add(1, Ordering::Relaxed));
        self.send(json!({
            "t": "command", "seq": self.seq, "idempotency_key": key,
            "issued_at": "2026-10-27T22:01:00.000Z", "command": command,
        }))
        .await;
        key
    }

    async fn dispatch(&mut self, thread: &str, task: &str, agent: &str, path: &Path) -> String {
        self.dispatch_in(thread, task, agent, json!({ "path": path }))
            .await
    }

    async fn dispatch_in(
        &mut self,
        thread: &str,
        task: &str,
        agent: &str,
        workspace: Value,
    ) -> String {
        self.dispatch_with_mode(thread, task, agent, workspace, "default")
            .await
    }

    async fn dispatch_with_mode(
        &mut self,
        thread: &str,
        task: &str,
        agent: &str,
        workspace: Value,
        mode: &str,
    ) -> String {
        self.command(json!({
            "kind": "dispatch", "thread_id": thread, "task_id": task, "agent": agent,
            "workspace": workspace, "permission_mode": mode, "message_id": format!("msg_{task}"),
            "parts": [{ "type": "text", "text": "hi" }],
            "author": { "kind": "user", "id": "usr_1", "device_id": "dev_1" },
        }))
        .await
    }

    async fn prompt(&mut self, thread: &str, task: &str) -> String {
        self.command(json!({
            "kind": "prompt", "thread_id": thread, "task_id": task, "message_id": format!("msg_{task}"),
            "parts": [{ "type": "text", "text": "more" }],
            "author": { "kind": "user", "id": "usr_1", "device_id": "dev_1" },
        }))
        .await
    }
}

fn event_type(frame: &Value) -> Option<&str> {
    frame["envelope"]["event"]["type"].as_str()
}

fn is_event(frame: &Value, ty: &str, task: &str) -> bool {
    event_type(frame) == Some(ty) && frame["envelope"]["task_id"] == task
}

fn run_finished(task: &'static str) -> impl Fn(&Value) -> bool {
    move |f| is_event(f, "RunFinished", task)
}

fn has_text(frames: &[Value], text: &str) -> bool {
    frames.iter().any(|f| {
        event_type(f) == Some("TextMessageContent") && f["envelope"]["event"]["delta"] == text
    })
}

fn is_result(frame: &Value, key: &str) -> bool {
    frame["t"] == "command_result" && frame["idempotency_key"] == key
}

// ---- the rig ---------------------------------------------------------------------------------

fn one(text: &str) -> Value {
    json!({ "turns": [{ "steps": [{ "text": text }] }] })
}

/// An agent that pauses on an approval nobody answers.
fn pauses() -> Value {
    json!({ "turns": [{ "steps": [
        { "text": "one" },
        { "request_permission": {
            "toolCall": { "toolCallId": "call_1", "title": "Run tests", "kind": "execute" },
            "options": [{ "optionId": "allow_once", "name": "Allow", "kind": "allow_once" }] },
          "expect": "any" }
    ]}]})
}

struct Rig {
    _dir: TempDir,
    root: PathBuf,
    hub: Hub,
}

impl Rig {
    /// `scripts[i]` serves the (i+1)th agent process.
    async fn new(scripts: &[Value]) -> Self {
        let dir = TempDir::new().unwrap();
        let root = dir.path().canonicalize().unwrap();
        for sub in ["state", "home", "bin", "work", "mock"] {
            fs::create_dir_all(root.join(sub)).unwrap();
        }
        for (i, script) in scripts.iter().enumerate() {
            fs::write(
                root.join(format!("mock/script-{}.json", i + 1)),
                script.to_string(),
            )
            .unwrap();
        }
        // `opencode acp` is how discovery finds the stub; the arguments are ignored.
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
        let hub = Hub::start().await;
        let rig = Self {
            _dir: dir,
            root,
            hub,
        };
        CredentialStore::file_only(rig.root.join("credential.json"))
            .save(&Login {
                hub: rig.hub.base(),
                machine_id: "mch_test".to_owned(),
                credential: "smm_test_credential".to_owned(),
            })
            .unwrap();
        // The directories the tests dispatch into are trusted, as a user who ran `sm trust` would
        // have them (`repo()` adds its own).
        TrustStore::new(&rig.state(), &rig.root.join("home"))
            .trust(&rig.work())
            .unwrap();
        rig
    }

    fn state(&self) -> PathBuf {
        self.root.join("state")
    }

    fn work(&self) -> PathBuf {
        self.root.join("work")
    }

    fn options(&self) -> Options {
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
            asker: None,
            allow_remote_terminal: false,
        }
    }

    /// The agent pids in start order.
    fn pids(&self) -> Vec<u32> {
        fs::read_to_string(self.root.join("mock/pids"))
            .unwrap_or_default()
            .lines()
            .filter_map(|l| l.parse().ok())
            .collect()
    }

    /// A git repository with one commit in `work/repo`.
    fn repo(&self) -> PathBuf {
        let repo = self.work().join("repo");
        fs::create_dir_all(&repo).unwrap();
        for args in [
            vec!["init", "-q"],
            vec![
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "commit",
                "-q",
                "--allow-empty",
                "-m",
                "init",
            ],
        ] {
            let status = Process::new("git")
                .arg("-C")
                .arg(&repo)
                .args(args)
                .status()
                .unwrap();
            assert!(status.success());
        }
        TrustStore::new(&self.state(), &self.root.join("home"))
            .trust(&repo)
            .unwrap();
        repo
    }
}

struct Running {
    stop: oneshot::Sender<()>,
    task: JoinHandle<anyhow::Result<()>>,
}

fn start(options: Options) -> Running {
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::run(options, async {
        let _ = rx.await;
    }));
    Running { stop, task }
}

impl Running {
    async fn stop(self) {
        let _ = self.stop.send(());
        tokio::time::timeout(WAIT, self.task)
            .await
            .expect("the server stopped")
            .unwrap()
            .expect("the server ended cleanly");
    }
}

fn worktrees(repo: &Path) -> usize {
    let out = Process::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "list", "--porcelain"])
        .output()
        .unwrap();
    String::from_utf8_lossy(&out.stdout)
        .lines()
        .filter(|l| l.starts_with("worktree "))
        .count()
}

// ---- tests -----------------------------------------------------------------------------------

#[tokio::test]
async fn hello_advertises_the_discovered_harnesses_capacity_and_spawn_modes() {
    let rig = Rig::new(&[one("x")]).await;
    let mut options = rig.options();
    options.capacity = 7;
    let running = start(options);
    let (_conn, hello) = rig.hub.accept().await;
    let caps = &hello["capabilities"];
    assert_eq!(caps["harnesses"], json!(["opencode"]));
    assert_eq!(caps["max_sessions"], 7);
    assert_eq!(caps["spawn"], json!(["same_dir", "worktree"]));
    assert_eq!(caps["host_mode"], "local");
    assert_eq!(hello["machine_id"], "mch_test");
    running.stop().await;
}

#[tokio::test]
async fn a_dispatch_runs_a_session_and_its_events_reach_the_hub() {
    let rig = Rig::new(&[one("hello from the agent")]).await;
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    let frames = conn.until(run_finished("tk_1")).await;

    assert!(has_text(&frames, "hello from the agent"), "{frames:#?}");
    assert!(frames.iter().any(|f| is_event(f, "RunStarted", "tk_1")));
    // The hub dispatched the thread: the runner does not announce it with thread_open.
    assert!(frames.iter().all(|f| f["t"] != "thread_open"));
    // The dispatch frame is acked once the session processed it.
    let acked = |f: &Value| f["t"] == "ack" && f["seq"] == 1;
    if !frames.iter().any(acked) {
        conn.until(acked).await;
    }

    // Names are prefix-word-word and the session is the project's last one.
    let last = Projects::new(&rig.state())
        .last_session(&rig.work())
        .expect("saved");
    assert_eq!(last.thread_id, "th_1");
    assert_eq!(last.harness, "opencode");
    assert!(
        last.name.starts_with("tst-") && last.name.split('-').count() == 3,
        "{}",
        last.name
    );
    running.stop().await;
}

/// `start`, with the session links going to `sink`.
fn start_announcing(options: Options, sink: server::Sink) -> Running {
    let (stop, rx) = oneshot::channel::<()>();
    let task = tokio::spawn(server::run_announcing(options, sink, async {
        let _ = rx.await;
    }));
    Running { stop, task }
}

#[tokio::test]
async fn a_started_session_hands_its_link_to_the_sink() {
    let rig = Rig::new(&[one("hello")]).await;
    let (sink, printed) = collecting_sink();
    let running = start_announcing(rig.options(), sink);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(run_finished("tk_1")).await;

    // The hub is the tunnel's ws form; the link is its http form, the thread's deep link.
    let hub = rig.hub.base().replacen("ws://", "http://", 1);
    assert_eq!(until_len(&printed, 1).await, [format!("{hub}/t/th_1")]);
    running.stop().await;
}

#[tokio::test]
async fn the_permission_mode_is_set_on_the_session() {
    let mut script = one("done");
    script["modes"] = json!({
        "currentModeId": "default",
        "availableModes": [{ "id": "default", "name": "Default" }, { "id": "plan", "name": "Plan" }]
    });
    let rig = Rig::new(&[script]).await;
    let mut options = rig.options();
    options.permission_mode = PermissionMode::Plan;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    let frames = conn
        .until(|f| event_type(f) == Some("StateDelta") && f.to_string().contains("plan"))
        .await;
    assert!(!frames.is_empty());
    running.stop().await;
}

fn plan_modes(script: &mut Value) {
    script["modes"] = json!({
        "currentModeId": "default",
        "availableModes": [{ "id": "default", "name": "Default" }, { "id": "plan", "name": "Plan" }]
    });
}

/// What the first agent received, in order, as `method` (`method:modeId` for `session/set_mode`),
/// once `until` holds for it.
async fn received(rig: &Rig, until: impl Fn(&[String]) -> bool) -> Vec<String> {
    received_by(rig, 1, until).await
}

/// [`received`] for agent process `agent` (counted from 1 in start order).
async fn received_by(rig: &Rig, agent: usize, until: impl Fn(&[String]) -> bool) -> Vec<String> {
    let path = rig.root.join(format!("mock/recorded-{agent}.json"));
    let mut methods = Vec::new();
    for _ in 0..500 {
        methods = fs::read_to_string(&path)
            .ok()
            .and_then(|text| serde_json::from_str::<Vec<Value>>(&text).ok())
            .unwrap_or_default()
            .iter()
            .filter_map(|m| {
                let method = m["method"].as_str()?;
                Some(match m["params"]["modeId"].as_str() {
                    Some(mode) => format!("{method}:{mode}"),
                    None => method.to_owned(),
                })
            })
            .collect();
        if until(&methods) {
            return methods;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    panic!("the agent did not receive what was expected: {methods:?}");
}

#[tokio::test]
async fn the_permission_mode_is_in_force_before_the_first_prompt() {
    let mut script = one("done");
    plan_modes(&mut script);
    let rig = Rig::new(&[script]).await;
    let mut options = rig.options();
    options.permission_mode = PermissionMode::Plan;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(run_finished("tk_1")).await;
    let methods = received(&rig, |m| m.iter().any(|x| x == "session/prompt")).await;
    let at = |name: &str| methods.iter().position(|m| m == name).unwrap();
    assert!(
        at("session/set_mode:plan") < at("session/prompt"),
        "{methods:?}"
    );
    running.stop().await;
}

fn all_modes(script: &mut Value) {
    script["modes"] = json!({
        "currentModeId": "default",
        "availableModes": [
            { "id": "default", "name": "Default" }, { "id": "plan", "name": "Plan" },
            { "id": "acceptEdits", "name": "Accept edits" },
            { "id": "bypassPermissions", "name": "Bypass" },
        ]
    });
}

/// Runs one dispatch asking for `requested` on a machine whose local mode is `local`, and
/// returns what the agent received up to its first prompt.
async fn modes_received(local: PermissionMode, requested: &str) -> Vec<String> {
    let mut script = one("done");
    all_modes(&mut script);
    let rig = Rig::new(&[script]).await;
    let mut options = rig.options();
    options.permission_mode = local;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch_with_mode(
        "th_1",
        "tk_1",
        "opencode",
        json!({ "path": rig.work() }),
        requested,
    )
    .await;
    conn.until(run_finished("tk_1")).await;
    let methods = received(&rig, |m| m.iter().any(|x| x == "session/prompt")).await;
    running.stop().await;
    methods
}

#[tokio::test]
async fn a_remote_bypass_request_on_a_default_machine_runs_in_default() {
    let methods = modes_received(PermissionMode::Default, "bypassPermissions").await;
    assert!(
        !methods.iter().any(|m| m.starts_with("session/set_mode")),
        "the agent stays in default: {methods:?}"
    );
    let methods = modes_received(PermissionMode::Default, "acceptEdits").await;
    assert!(
        !methods.iter().any(|m| m.starts_with("session/set_mode")),
        "{methods:?}"
    );
}

#[tokio::test]
async fn a_remote_plan_request_runs_in_plan() {
    let methods = modes_received(PermissionMode::Default, "plan").await;
    let at = |name: &str| methods.iter().position(|m| m == name).unwrap();
    assert!(
        at("session/set_mode:plan") < at("session/prompt"),
        "{methods:?}"
    );
}

#[tokio::test]
async fn a_request_is_capped_by_the_local_mode_in_both_directions() {
    // Looser than a plan machine: plan. Equal: kept. Stricter than acceptEdits: kept.
    let methods = modes_received(PermissionMode::Plan, "default").await;
    assert!(
        methods.contains(&"session/set_mode:plan".to_owned()),
        "{methods:?}"
    );
    let methods = modes_received(PermissionMode::AcceptEdits, "bypassPermissions").await;
    assert!(
        methods.contains(&"session/set_mode:acceptEdits".to_owned()),
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|m| m.contains("bypassPermissions")),
        "{methods:?}"
    );
    let methods = modes_received(PermissionMode::AcceptEdits, "plan").await;
    assert!(
        methods.contains(&"session/set_mode:plan".to_owned()),
        "{methods:?}"
    );
}

#[test]
fn the_cap_orders_plan_default_accept_edits_bypass() {
    use PermissionMode::{AcceptEdits, BypassPermissions, Default, Plan};
    let order = [Plan, Default, AcceptEdits, BypassPermissions];
    for (i, local) in order.iter().enumerate() {
        for (j, requested) in order.iter().enumerate() {
            let expected = order[i.min(j)];
            assert_eq!(local.capped(requested.as_str()), expected);
        }
        assert_eq!(local.capped("nonsense"), *local);
    }
}

#[tokio::test]
async fn hello_advertises_the_local_permission_mode() {
    let rig = Rig::new(&[one("x")]).await;
    let mut options = rig.options();
    options.permission_mode = PermissionMode::AcceptEdits;
    let running = start(options);
    let (_conn, hello) = rig.hub.accept().await;
    assert!(
        hello["capabilities"]["features"]
            .as_array()
            .unwrap()
            .contains(&json!("permission_mode:acceptEdits")),
        "{hello}"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_mode_the_agent_does_not_advertise_fails_the_dispatch_without_a_prompt() {
    // The agent has no `plan` mode.
    let rig = Rig::new(&[one("must not run")]).await;
    let mut options = rig.options();
    options.permission_mode = PermissionMode::Plan;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    let frames = conn.until(|f| is_event(f, "RunError", "tk_1")).await;
    assert!(!has_text(&frames, "must not run"));
    let methods = received(&rig, |m| m.iter().any(|x| x == "session/new")).await;
    assert!(
        !methods.iter().any(|m| m == "session/prompt"),
        "{methods:?}"
    );
    assert!(
        !methods.iter().any(|m| m.starts_with("session/set_mode")),
        "{methods:?}"
    );
    running.stop().await;
}

#[tokio::test]
async fn http_and_sse_mcp_servers_are_passed_when_the_agent_advertises_them() {
    let mut script = one("x");
    script["mcp_capabilities"] = json!({ "http": true, "sse": true });
    let rig = Rig::new(&[script]).await;
    fs::write(
        rig.work().join(".mcp.json"),
        json!({ "mcpServers": {
            "remote": { "type": "http", "url": "https://example.test/mcp" },
            "events": { "type": "sse", "url": "https://example.test/sse" },
        }})
        .to_string(),
    )
    .unwrap();
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;
    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(run_finished("tk_1")).await;
    received(&rig, |m| m.iter().any(|x| x == "session/prompt")).await;

    let text = fs::read_to_string(rig.root.join("mock/recorded-1.json")).unwrap();
    let messages: Vec<Value> = serde_json::from_str(&text).unwrap();
    let new = messages
        .iter()
        .find(|m| m["method"] == "session/new")
        .expect("session/new");
    let mut types: Vec<&str> = new["params"]["mcpServers"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|s| s["type"].as_str())
        .collect();
    types.sort_unstable();
    assert_eq!(types, ["http", "sse"], "{new}");
    running.stop().await;
}

#[tokio::test]
async fn http_mcp_servers_are_left_out_when_the_agent_does_not_advertise_them() {
    let rig = Rig::new(&[one("x")]).await;
    fs::write(
        rig.work().join(".mcp.json"),
        json!({ "mcpServers": { "remote": { "type": "http", "url": "https://example.test/mcp" } }})
            .to_string(),
    )
    .unwrap();
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;
    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(run_finished("tk_1")).await;
    received(&rig, |m| m.iter().any(|x| x == "session/prompt")).await;
    let text = fs::read_to_string(rig.root.join("mock/recorded-1.json")).unwrap();
    let messages: Vec<Value> = serde_json::from_str(&text).unwrap();
    let new = messages.iter().find(|m| m["method"] == "session/new");
    assert_eq!(new.unwrap()["params"]["mcpServers"], json!([]));
    running.stop().await;
}

/// A worktree of `repo` at `rig.state()/worktrees/<name>`.
fn add_worktree(rig: &Rig, repo: &Path, name: &str) -> PathBuf {
    let path = rig.state().join("worktrees").join(name);
    fs::create_dir_all(path.parent().unwrap()).unwrap();
    let status = Process::new("git")
        .arg("-C")
        .arg(repo)
        .args(["worktree", "add", "--detach"])
        .arg(&path)
        .arg("HEAD")
        .status()
        .unwrap();
    assert!(status.success());
    path
}

#[tokio::test]
async fn stale_worktrees_are_swept_at_start_clean_ones_removed_and_dirty_ones_kept() {
    let rig = Rig::new(&[one("x")]).await;
    let repo = rig.repo();
    let clean = add_worktree(&rig, &repo, "left-clean");
    let dirty = add_worktree(&rig, &repo, "left-dirty");
    fs::write(dirty.join("work.txt"), "unsaved").unwrap();
    assert_eq!(worktrees(&repo), 3);

    let running = start(rig.options());
    let (_conn, _) = rig.hub.accept().await;
    assert!(!clean.exists(), "the clean leftover is removed");
    assert!(dirty.join("work.txt").exists(), "the dirty one is kept");
    assert_eq!(worktrees(&repo), 2);
    running.stop().await;
}

#[tokio::test]
async fn a_cancel_of_an_idle_thread_ends_the_session_and_removes_its_worktree() {
    let rig = Rig::new(&[one("a"), one("b")]).await;
    let repo = rig.repo();
    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    options.capacity = 1;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &repo).await;
    conn.until(run_finished("tk_1")).await;
    assert_eq!(worktrees(&repo), 2);

    let key = conn
        .command(json!({ "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }))
        .await;
    conn.until(|f| f["t"] == "ack" && f["seq"] == 2).await;
    assert_eq!(worktrees(&repo), 1, "the worktree is removed");
    assert!(!key.is_empty());
    // The agent process is gone with the session.
    let pid = rig.pids()[0];
    let mut gone = false;
    for _ in 0..200 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            gone = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    assert!(gone, "the agent process is stopped");

    // The slot is free, and the thread can be prompted again (a new session over its store).
    conn.dispatch("th_2", "tk_2", "opencode", &repo).await;
    conn.until(run_finished("tk_2")).await;
    running.stop().await;
}

/// The one worktree under `state/worktrees`.
fn session_worktree(rig: &Rig) -> PathBuf {
    let made: Vec<PathBuf> = fs::read_dir(rig.state().join("worktrees"))
        .unwrap()
        .map(|entry| entry.unwrap().path())
        .collect();
    assert_eq!(made.len(), 1, "{made:?}");
    made[0].clone()
}

/// The output of `git -C dir args`, trimmed.
fn git_out(dir: &Path, args: &[&str]) -> String {
    let out = Process::new("git")
        .arg("-C")
        .arg(dir)
        .args(args)
        .output()
        .unwrap();
    assert!(out.status.success(), "git {args:?}");
    String::from_utf8_lossy(&out.stdout).trim().to_owned()
}

/// Whether the agent process `pid` has gone (within a few seconds).
async fn process_gone(pid: u32) -> bool {
    for _ in 0..200 {
        if !Path::new(&format!("/proc/{pid}")).exists() {
            return true;
        }
        tokio::time::sleep(Duration::from_millis(25)).await;
    }
    false
}

#[tokio::test]
async fn a_prompt_after_an_idle_close_adds_the_worktree_back_on_its_branch_and_resumes_the_session()
{
    let rig = Rig::new(&[one("a"), one("b")]).await;
    let repo = rig.repo();
    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &repo).await;
    conn.until(run_finished("tk_1")).await;
    let path = session_worktree(&rig);
    let branch = git_out(&path, &["rev-parse", "--abbrev-ref", "HEAD"]);
    assert!(branch.starts_with("sm/"), "{branch}");
    let branches = git_out(&repo, &["branch", "--format=%(refname:short)"]);

    // The hub closes the idle thread: its worktree goes, the branch stays.
    conn.command(json!({ "kind": "cancel", "thread_id": "th_1", "task_id": "tk_1" }))
        .await;
    conn.until(|f| f["t"] == "ack" && f["seq"] == 2).await;
    assert!(!path.exists(), "the worktree is removed");
    assert_eq!(worktrees(&repo), 1);

    // The next prompt reopens the session: the worktree is back on the same branch, and the
    // agent resumes its session in it.
    conn.prompt("th_1", "tk_2").await;
    let frames = conn.until(run_finished("tk_2")).await;
    assert!(has_text(&frames, "b"), "{frames:#?}");
    assert!(path.join(".git").exists(), "the worktree is back");
    assert_eq!(worktrees(&repo), 2);
    assert_eq!(
        git_out(&path, &["rev-parse", "--abbrev-ref", "HEAD"]),
        branch
    );
    assert_eq!(
        git_out(&repo, &["branch", "--format=%(refname:short)"]),
        branches,
        "no new branch"
    );

    let methods = received_by(&rig, 2, |m| m.iter().any(|x| x == "session/prompt")).await;
    assert!(
        methods
            .iter()
            .any(|m| m == "session/resume" || m == "session/load"),
        "{methods:?}"
    );
    assert!(!methods.iter().any(|m| m == "session/new"), "{methods:?}");
    assert_eq!(
        rig.pids().len(),
        2,
        "a new agent process resumed the session"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_local_close_ends_an_idle_session_and_the_next_prompt_reopens_it() {
    let rig = Rig::new(&[one("a"), one("b")]).await;
    let repo = rig.repo();
    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &repo).await;
    conn.until(run_finished("tk_1")).await;
    let path = session_worktree(&rig);
    let pid = rig.pids()[0];

    server::close_session(&rig.state(), "th_1")
        .await
        .expect("the session is closed");
    assert!(!path.exists(), "the worktree is removed");
    assert_eq!(worktrees(&repo), 1);
    assert!(process_gone(pid).await, "the agent process is stopped");
    let socket = rig.state().join("rc.sock");
    let mode = fs::metadata(&socket).unwrap().permissions().mode() & 0o777;
    assert_eq!(mode, 0o600, "the control socket is owner-only");

    conn.prompt("th_1", "tk_2").await;
    let frames = conn.until(run_finished("tk_2")).await;
    assert!(has_text(&frames, "b"), "{frames:#?}");
    assert_eq!(worktrees(&repo), 2);

    running.stop().await;
    assert!(
        !socket.exists(),
        "the socket is removed when the server stops"
    );
}

#[tokio::test]
async fn a_local_close_of_a_running_session_or_an_unknown_thread_is_refused() {
    let rig = Rig::new(&[pauses()]).await;
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(|f| event_type(f) == Some("Interrupt")).await;
    let busy = server::close_session(&rig.state(), "th_1")
        .await
        .expect_err("a task is running");
    assert!(busy.to_string().contains("has a task running"), "{busy:#}");
    let unknown = server::close_session(&rig.state(), "th_9")
        .await
        .expect_err("no such session");
    assert!(
        unknown.to_string().contains("no open session"),
        "{unknown:#}"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_session_ends_after_the_idle_timeout() {
    let rig = Rig::new(&[one("a")]).await;
    let repo = rig.repo();
    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    options.idle_timeout = Duration::from_millis(400);
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &repo).await;
    conn.until(run_finished("tk_1")).await;
    assert_eq!(worktrees(&repo), 2);
    let mut removed = false;
    for _ in 0..200 {
        if worktrees(&repo) == 1 {
            removed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(removed, "the idle session's worktree is removed");
    running.stop().await;
}

#[tokio::test]
async fn a_dispatch_beyond_capacity_fails_with_capacity() {
    let rig = Rig::new(&[pauses()]).await;
    let mut options = rig.options();
    options.capacity = 1;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(|f| event_type(f) == Some("Interrupt")).await;
    let key = conn.dispatch("th_2", "tk_2", "opencode", &rig.work()).await;
    let frames = conn.until(|f| is_result(f, &key)).await;
    let result = frames.last().unwrap();
    assert_eq!(result["ok"], false);
    assert_eq!(result["code"], "capacity");
    assert!(
        !rig.state().join("sessions/th_2").exists(),
        "no session was started"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_dispatch_for_a_harness_that_is_not_installed_fails_with_harness_missing() {
    let rig = Rig::new(&[one("x")]).await;
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;

    let key = conn.dispatch("th_1", "tk_1", "claude", &rig.work()).await;
    let frames = conn.until(|f| is_result(f, &key)).await;
    assert_eq!(frames.last().unwrap()["code"], "harness_missing");
    running.stop().await;
}

#[tokio::test]
async fn a_command_for_an_unknown_thread_fails_with_unknown_thread() {
    let rig = Rig::new(&[one("x")]).await;
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;

    let key = conn.prompt("th_nope", "tk_1").await;
    let frames = conn.until(|f| is_result(f, &key)).await;
    assert_eq!(frames.last().unwrap()["code"], "unknown_thread");
    running.stop().await;
}

#[tokio::test]
async fn worktree_spawn_adds_a_worktree_per_session_and_removes_it_when_the_session_ends() {
    let rig = Rig::new(&[one("in a worktree")]).await;
    let repo = rig.repo();
    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;
    assert_eq!(worktrees(&repo), 1);

    conn.dispatch("th_1", "tk_1", "opencode", &repo).await;
    let frames = conn.until(run_finished("tk_1")).await;
    assert!(has_text(&frames, "in a worktree"));

    let root = rig.state().join("worktrees");
    let made: Vec<_> = fs::read_dir(&root).unwrap().collect();
    assert_eq!(made.len(), 1, "one worktree for the session");
    let path = made[0].as_ref().unwrap().path();
    assert!(path.join(".git").exists());
    assert_eq!(worktrees(&repo), 2);
    // The session runs in it.
    let store = sidecar::store::Store::open(&rig.state().join("sessions/th_1")).unwrap();
    assert_eq!(
        store.thread("th_1").unwrap().unwrap().cwd,
        path.to_string_lossy()
    );
    drop(store);

    running.stop().await;
    assert!(!path.exists(), "the worktree is removed with the session");
    assert_eq!(worktrees(&repo), 1);
}

#[tokio::test]
async fn the_spawn_mode_is_saved_per_project_and_reused() {
    let rig = Rig::new(&[one("a"), one("b")]).await;
    let repo = rig.repo();

    let mut options = rig.options();
    options.spawn = Some(SpawnMode::Worktree);
    options.cwd = repo.clone();
    let running = start(options);
    let (_conn, hello) = rig.hub.accept().await;
    assert_eq!(hello["capabilities"]["spawn"][0], "worktree");
    running.stop().await;
    assert_eq!(
        Projects::new(&rig.state()).spawn_mode(&repo),
        Some(SpawnMode::Worktree)
    );

    // No --spawn this time: the saved mode applies to a dispatch that names none.
    let mut options = rig.options();
    options.cwd = repo.clone();
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;
    conn.dispatch("th_2", "tk_2", "opencode", &repo).await;
    conn.until(run_finished("tk_2")).await;
    assert_eq!(
        fs::read_dir(rig.state().join("worktrees")).unwrap().count(),
        1
    );
    running.stop().await;

    // Another directory has no saved mode.
    assert_eq!(Projects::new(&rig.state()).spawn_mode(&rig.work()), None);
}

#[tokio::test]
async fn continue_registers_the_last_session_of_the_directory_and_resumes_it() {
    let rig = Rig::new(&[one("one"), one("two")]).await;

    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;
    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    let frames = conn.until(run_finished("tk_1")).await;
    // Ack everything, so the second run does not send the first run's events again.
    let last = frames
        .iter()
        .filter_map(|f| f["seq"].as_u64())
        .max()
        .unwrap();
    conn.send(json!({ "t": "ack", "seq": last })).await;
    tokio::time::sleep(Duration::from_millis(300)).await;
    running.stop().await;

    let mut options = rig.options();
    options.continue_last = true;
    let running = start(options);
    let (mut conn, _) = rig.hub.accept().await;
    // The thread is announced as started on this runner before anything else for it.
    let frames = conn.until(|f| f["t"] == "thread_open").await;
    let open = frames.last().unwrap();
    assert_eq!(open["thread_id"], "th_1");
    assert_eq!(open["agent"], "opencode");
    assert_eq!(
        open["workspace"]["path"],
        rig.work().to_string_lossy().as_ref()
    );
    assert!(open["title"].as_str().unwrap().starts_with("tst-"));
    assert!(
        frames[..frames.len() - 1]
            .iter()
            .all(|f| f["envelope"]["thread_id"] != "th_1")
    );

    conn.prompt("th_1", "tk_2").await;
    let frames = conn.until(run_finished("tk_2")).await;
    assert!(
        has_text(&frames, "two"),
        "the continued session answered: {frames:#?}"
    );
    assert_eq!(
        rig.pids().len(),
        2,
        "a new agent process resumed the session"
    );
    running.stop().await;
}

#[tokio::test]
async fn a_killed_agent_is_started_again_and_resumed_on_the_next_prompt() {
    let rig = Rig::new(&[pauses(), one("two")]).await;
    let running = start(rig.options());
    let (mut conn, _) = rig.hub.accept().await;

    conn.dispatch("th_1", "tk_1", "opencode", &rig.work()).await;
    conn.until(|f| event_type(f) == Some("Interrupt")).await;
    let pids = rig.pids();
    assert_eq!(pids.len(), 1);
    let killed = Process::new("kill")
        .args(["-KILL", &pids[0].to_string()])
        .status()
        .unwrap();
    assert!(killed.success());

    let frames = conn.until(|f| is_event(f, "RunError", "tk_1")).await;
    assert_eq!(
        frames.last().unwrap()["envelope"]["event"]["code"],
        "harness_exited"
    );

    conn.prompt("th_1", "tk_2").await;
    let frames = conn.until(run_finished("tk_2")).await;
    assert!(has_text(&frames, "two"), "{frames:#?}");
    let pids = rig.pids();
    assert_eq!(pids.len(), 2, "one respawn");
    running.stop().await;
}

#[test]
fn rc_flags_parse_with_defaults() {
    let args = |list: &[&str]| list.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>();
    let defaults = RcArgs::parse(&[]).unwrap();
    assert_eq!(defaults.capacity, 32);
    assert_eq!(defaults.permission_mode, PermissionMode::Default);
    assert_eq!(defaults.spawn, None);
    assert!(!defaults.continue_last);
    assert_eq!(defaults.idle_timeout, Duration::from_secs(7200));

    let all = RcArgs::parse(&args(&[
        "--spawn",
        "worktree",
        "--capacity",
        "4",
        "--permission-mode",
        "acceptEdits",
        "--name-prefix",
        "Box",
        "--idle-timeout",
        "15m",
        "-c",
    ]))
    .unwrap();
    assert_eq!(all.idle_timeout, Duration::from_secs(900));
    assert_eq!(all.spawn, Some(SpawnMode::Worktree));
    assert_eq!(all.capacity, 4);
    assert_eq!(all.permission_mode, PermissionMode::AcceptEdits);
    assert_eq!(all.name_prefix.as_deref(), Some("box"));
    assert!(all.continue_last);
    assert!(RcArgs::parse(&args(&["--continue"])).unwrap().continue_last);
    assert_eq!(
        RcArgs::parse(&args(&["--spawn", "same-dir"]))
            .unwrap()
            .spawn,
        Some(SpawnMode::SameDir)
    );

    for bad in [
        vec!["--spawn", "moon"],
        vec!["--capacity", "0"],
        vec!["--permission-mode", "yolo"],
        vec!["--idle-timeout", "0"],
        vec!["--idle-timeout", "2d"],
        vec!["--capacity"],
        vec!["--nope"],
    ] {
        assert!(RcArgs::parse(&args(&bad)).is_err(), "{bad:?}");
    }
}

// ---- the phone sign-in link (M2-003c) ----------------------------------------------------------

/// What the fake hub saw on `POST /v1/devices/signin-links`.
#[derive(Default)]
struct LinkLog {
    /// `Authorization` header of each request.
    authorizations: Vec<String>,
    paths: Vec<String>,
}

/// A hub that serves the tunnel (WebSocket) and `POST /v1/devices/signin-links` on one port, as
/// the real one does. Link `n` is `sli_<n as 64 hex digits>`; a hub with `device_origin: false`
/// answers `403 forbidden`, as for a machine the admin API created.
struct LinkHub {
    base: String,
    log: Arc<Mutex<LinkLog>>,
    task: JoinHandle<()>,
}

impl LinkHub {
    async fn start(device_origin: bool) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("ws://{}", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(LinkLog::default()));
        let seen = Arc::clone(&log);
        let task = tokio::spawn(async move {
            loop {
                let Ok((stream, _)) = listener.accept().await else {
                    return;
                };
                let seen = Arc::clone(&seen);
                tokio::spawn(async move {
                    let mut head = [0u8; 5];
                    if stream.peek(&mut head).await.is_err() {
                        return;
                    }
                    if head.starts_with(b"POST ") {
                        serve_link(stream, &seen, device_origin).await;
                    } else {
                        serve_tunnel(stream).await;
                    }
                });
            }
        });
        Self { base, log, task }
    }

    fn requests(&self) -> usize {
        self.log.lock().unwrap().paths.len()
    }
}

impl Drop for LinkHub {
    fn drop(&mut self) {
        self.task.abort();
    }
}

/// Accepts the runner's tunnel, answers `hello` with `welcome` and then just reads.
async fn serve_tunnel(stream: TcpStream) {
    let Ok(mut ws) = tokio_tungstenite::accept_hdr_async(stream, Echo).await else {
        return;
    };
    while let Some(Ok(message)) = ws.next().await {
        if let Message::Text(text) = message
            && serde_json::from_str::<Value>(text.as_str()).is_ok_and(|frame| frame["t"] == "hello")
        {
            let welcome = json!({
                "t": "welcome", "proto": 1, "resumed": false, "resume_token": "tok",
                "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": 256,
            });
            let _ = ws.send(Message::text(welcome.to_string())).await;
        }
    }
}

async fn serve_link(mut stream: TcpStream, seen: &Mutex<LinkLog>, device_origin: bool) {
    use tokio::io::AsyncReadExt;
    let mut raw = Vec::new();
    let mut chunk = [0u8; 1024];
    while !raw.windows(4).any(|w| w == b"\r\n\r\n") {
        match stream.read(&mut chunk).await {
            Ok(0) | Err(_) => return,
            Ok(n) => raw.extend_from_slice(&chunk[..n]),
        }
    }
    let head = String::from_utf8_lossy(&raw).into_owned();
    let path = head
        .lines()
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .unwrap_or_default()
        .to_owned();
    let authorization = head
        .lines()
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("authorization")
                .then(|| value.trim().to_owned())
        })
        .unwrap_or_default();
    let n = {
        let mut log = seen.lock().unwrap();
        log.authorizations.push(authorization);
        log.paths.push(path);
        log.paths.len()
    };
    let (status, body) = if device_origin {
        (
            "201 Created",
            json!({
                "url": format!("https://hub.example/pair?code=sli_{n:064x}"),
                "expires_at": "2030-01-01T00:00:00.000Z",
            }),
        )
    } else {
        (
            "403 Forbidden",
            json!({ "error": "forbidden", "message": "only a machine a member approved" }),
        )
    };
    let body = body.to_string();
    let response = format!(
        "HTTP/1.1 {status}\r\ncontent-type: application/json\r\ncontent-length: {}\r\n\
         connection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes()).await;
    let _ = stream.shutdown().await;
}

/// Everything `tracing` was told, as text, to prove a secret never reached it.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<String>>);

struct Fields<'a>(&'a Mutex<String>);

impl tracing::field::Visit for Fields<'_> {
    fn record_debug(&mut self, field: &tracing::field::Field, value: &dyn std::fmt::Debug) {
        self.0
            .lock()
            .unwrap()
            .push_str(&format!("{}={value:?} ", field.name()));
    }
}

impl tracing::Subscriber for Captured {
    fn enabled(&self, _: &tracing::Metadata<'_>) -> bool {
        true
    }
    fn new_span(&self, attrs: &tracing::span::Attributes<'_>) -> tracing::span::Id {
        attrs.record(&mut Fields(&self.0));
        tracing::span::Id::from_u64(1)
    }
    fn record(&self, _: &tracing::span::Id, values: &tracing::span::Record<'_>) {
        values.record(&mut Fields(&self.0));
    }
    fn record_follows_from(&self, _: &tracing::span::Id, _: &tracing::span::Id) {}
    fn event(&self, event: &tracing::Event<'_>) {
        self.0.lock().unwrap().push_str(event.metadata().name());
        event.record(&mut Fields(&self.0));
    }
    fn enter(&self, _: &tracing::span::Id) {}
    fn exit(&self, _: &tracing::span::Id) {}
}

fn collecting_sink() -> (server::Sink, Arc<Mutex<Vec<String>>>) {
    let printed = Arc::new(Mutex::new(Vec::new()));
    let into = Arc::clone(&printed);
    let sink: server::Sink = Arc::new(move |block: &str| {
        into.lock().unwrap().push(block.to_owned());
    });
    (sink, printed)
}

async fn until_len(printed: &Mutex<Vec<String>>, len: usize) -> Vec<String> {
    tokio::time::timeout(WAIT, async {
        loop {
            {
                let blocks = printed.lock().unwrap();
                if blocks.len() >= len {
                    return blocks.clone();
                }
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the sign-in link was not printed")
}

fn code_in(block: &str) -> String {
    let url = block
        .lines()
        .map(str::trim)
        .find(|line| line.starts_with("https://hub.example/pair?code="))
        .expect("the URL line");
    url.split_once("code=").unwrap().1.to_owned()
}

fn login_to(rig: &Rig, hub: &LinkHub) {
    CredentialStore::file_only(rig.root.join("credential.json"))
        .save(&Login {
            hub: hub.base.clone(),
            machine_id: "mch_test".to_owned(),
            credential: "smm_test_credential".to_owned(),
        })
        .unwrap();
}

#[tokio::test]
async fn rc_prints_the_sign_in_qr_and_url_at_start_and_on_sigusr1_and_never_logs_the_code() {
    let captured = Captured::default();
    let _tracing = tracing::subscriber::set_default(captured.clone());
    let rig = Rig::new(&[]).await;
    let hub = LinkHub::start(true).await;
    login_to(&rig, &hub);
    let (sink, printed) = collecting_sink();
    let mut options = rig.options();
    options.signin = Some(sink);
    let running = start(options);

    // At start: the QR code and the URL, from a link the machine asked for with its credential.
    let first = until_len(&printed, 1).await[0].clone();
    let code = code_in(&first);
    assert!(code.starts_with("sli_") && code.len() == 4 + 64, "{code}");
    let qr = runner::login::flow::qr_text(&format!("https://hub.example/pair?code={code}"))
        .expect("a QR code");
    assert!(
        first.contains(&qr),
        "the block carries the QR code of the URL"
    );
    assert!(
        first.contains(['\u{2580}', '\u{2584}', '\u{2588}']),
        "unicode blocks"
    );
    {
        let log = hub.log.lock().unwrap();
        assert_eq!(log.paths, ["/v1/devices/signin-links"]);
        assert_eq!(log.authorizations, ["Bearer smm_test_credential"]);
    }

    // SIGUSR1: a fresh link (the first one is single use), printed again.
    let sent = Process::new("kill")
        .args(["-USR1", &std::process::id().to_string()])
        .status()
        .unwrap();
    assert!(sent.success());
    let blocks = until_len(&printed, 2).await;
    let second = code_in(&blocks[1]);
    assert_ne!(second, code);
    let second_qr =
        runner::login::flow::qr_text(&format!("https://hub.example/pair?code={second}")).unwrap();
    assert!(blocks[1].contains(&second_qr));
    running.stop().await;

    // The codes live in the printed blocks only: not in tracing, not in the state directory.
    let logged = captured.0.lock().unwrap().clone();
    for secret in [&code, &second, &"smm_test_credential".to_owned()] {
        assert!(
            !logged.contains(secret.as_str()),
            "tracing carried a secret: {logged}"
        );
    }
    let mut dirs = vec![rig.state()];
    while let Some(dir) = dirs.pop() {
        for entry in fs::read_dir(dir).unwrap().flatten() {
            let path = entry.path();
            if path.is_dir() {
                dirs.push(path);
            } else if let Ok(text) = fs::read_to_string(&path) {
                assert!(
                    !text.contains(&code) && !text.contains(&second),
                    "{}",
                    path.display()
                );
            }
        }
    }
}

#[tokio::test]
async fn a_machine_the_hub_gives_no_link_prints_nothing() {
    let captured = Captured::default();
    let _tracing = tracing::subscriber::set_default(captured.clone());
    let rig = Rig::new(&[]).await;
    let hub = LinkHub::start(false).await;
    login_to(&rig, &hub);
    let (sink, printed) = collecting_sink();
    let mut options = rig.options();
    options.signin = Some(sink);
    let running = start(options);
    tokio::time::timeout(WAIT, async {
        while hub.requests() == 0 {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("the runner asked for a link");
    tokio::time::sleep(Duration::from_millis(300)).await;
    assert!(
        printed.lock().unwrap().is_empty(),
        "nothing to print for an admin-created machine"
    );
    running.stop().await;
    assert!(!captured.0.lock().unwrap().contains("smm_test_credential"));
}
