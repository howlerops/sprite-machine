//! The tunnel client against an in-process fake hub (a `tokio-tungstenite` server in the test).
//! The fake hub plays the Machine DO's side of tunnel.md closely enough to check the runner's
//! contract (docs/ledger/reports/M2B-002.md "Runner-side contract").

use std::net::SocketAddr;
use std::path::Path;
use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use anyhow::Result;
use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::{Capabilities, CloseCode, Command, Workspace};
use protocol::turn_event::Envelope;
use runner::tunnel::{
    Backoff, CredentialSource, Exit, Jitter, Timing, TunnelClient, TunnelCommands, TunnelConfig,
    TunnelSink, WallClock, tunnel,
};
use serde_json::{Value, json};
use sidecar::bridge::{
    Bridge, CommandFailure, CommandSource, Config, EventSink, SequentialGenerator,
};
use sidecar::store::Store;
use tempfile::TempDir;
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::WebSocketStream;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{
    Callback, ErrorResponse, Request, Response,
};
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::protocol::CloseFrame;
use tokio_tungstenite::tungstenite::protocol::frame::coding::CloseCode as WsCloseCode;

const CREDENTIAL: &str = "smm_test_credential";
const WAIT: Duration = Duration::from_secs(5);

// ---- fake hub -------------------------------------------------------------------------------

/// Ports of every fake hub in this test process (tests run in parallel).
static HUB_PORTS: Mutex<Vec<u16>> = Mutex::new(Vec::new());

/// What the upgrade request carried: path, `Authorization`, `Sec-WebSocket-Protocol`.
#[derive(Clone, Default)]
struct Seen(String, Option<String>, Option<String>);

struct Upgrade {
    seen: Arc<Mutex<Seen>>,
    refuse: Option<u16>,
}

impl Callback for Upgrade {
    fn on_request(self, req: &Request, mut resp: Response) -> Result<Response, ErrorResponse> {
        let header = |name| {
            req.headers()
                .get(name)
                .and_then(|v| v.to_str().ok())
                .map(str::to_owned)
        };
        *self.seen.lock().unwrap() = Seen(
            req.uri().to_string(),
            header(AUTHORIZATION),
            header(SEC_WEBSOCKET_PROTOCOL),
        );
        if let Some(status) = self.refuse {
            let mut refusal = ErrorResponse::new(None);
            *refusal.status_mut() = status.try_into().unwrap();
            return Err(refusal);
        }
        if let Some(protocol) = header(SEC_WEBSOCKET_PROTOCOL) {
            resp.headers_mut()
                .insert(SEC_WEBSOCKET_PROTOCOL, protocol.parse().unwrap());
        }
        Ok(resp)
    }
}

struct FakeHub {
    addr: SocketAddr,
    conns: mpsc::UnboundedReceiver<HubConn>,
}

struct HubConn {
    ws: WebSocketStream<TcpStream>,
    path: String,
    authorization: Option<String>,
    subprotocol: Option<String>,
    accepted_at: Instant,
}

impl FakeHub {
    async fn start() -> Self {
        Self::start_with(None).await
    }

    /// A hub that refuses every upgrade with `status`.
    async fn refusing(status: u16) -> Self {
        Self::start_with(Some(status)).await
    }

    async fn start_with(refuse: Option<u16>) -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        HUB_PORTS.lock().unwrap().push(addr.port());
        let (tx, conns) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            while let Ok((stream, _)) = listener.accept().await {
                let seen = Arc::new(Mutex::new(Seen::default()));
                let callback = Upgrade {
                    seen: Arc::clone(&seen),
                    refuse,
                };
                let Ok(ws) = tokio_tungstenite::accept_hdr_async(stream, callback).await else {
                    continue;
                };
                let Seen(path, authorization, subprotocol) = seen.lock().unwrap().clone();
                let _ = tx.send(HubConn {
                    ws,
                    path,
                    authorization,
                    subprotocol,
                    accepted_at: Instant::now(),
                });
            }
        });
        Self { addr, conns }
    }

    fn base(&self) -> String {
        format!("ws://{}", self.addr)
    }

    async fn next_conn(&mut self) -> HubConn {
        tokio::time::timeout(WAIT, self.conns.recv())
            .await
            .expect("the runner did not connect")
            .expect("hub stopped")
    }

    /// `None` when no connection arrives within `wait`.
    async fn try_next_conn(&mut self, wait: Duration) -> Option<HubConn> {
        tokio::time::timeout(wait, self.conns.recv())
            .await
            .ok()
            .flatten()
    }
}

impl HubConn {
    /// The next text message, or `None` after `wait` or when the socket is closed.
    async fn raw_within(&mut self, wait: Duration) -> Option<String> {
        loop {
            match tokio::time::timeout(wait, self.ws.next()).await {
                Err(_) | Ok(None) | Ok(Some(Err(_))) => return None,
                Ok(Some(Ok(Message::Text(text)))) => return Some(text.as_str().to_owned()),
                Ok(Some(Ok(Message::Close(_)))) => return None,
                Ok(Some(Ok(_))) => {}
            }
        }
    }

    async fn recv(&mut self) -> Value {
        let raw = self
            .raw_within(WAIT)
            .await
            .expect("the runner sent no frame");
        serde_json::from_str(&raw).unwrap()
    }

    async fn recv_within(&mut self, wait: Duration) -> Option<Value> {
        self.raw_within(wait)
            .await
            .map(|raw| serde_json::from_str(&raw).unwrap())
    }

    async fn send(&mut self, frame: Value) {
        self.ws
            .send(Message::text(frame.to_string()))
            .await
            .expect("hub send");
    }

    /// Reads `hello` and asserts it is the first frame.
    async fn hello(&mut self) -> Value {
        let hello = self.recv().await;
        assert_eq!(hello["t"], "hello", "the first frame must be hello");
        hello
    }

    async fn welcome(&mut self, resumed: bool, token: &str, last_up_seq: u64, heartbeat_ms: u64) {
        self.send(json!({
            "t": "welcome", "proto": 1, "resumed": resumed, "resume_token": token,
            "last_up_seq": last_up_seq, "heartbeat_ms": heartbeat_ms, "max_inflight": 256,
        }))
        .await;
    }

    /// A `welcome` that advertises a smaller send window.
    async fn welcome_with_window(&mut self, resumed: bool, token: &str, max_inflight: u64) {
        self.send(json!({
            "t": "welcome", "proto": 1, "resumed": resumed, "resume_token": token,
            "last_up_seq": 0, "heartbeat_ms": 20_000, "max_inflight": max_inflight,
        }))
        .await;
    }

    async fn ack(&mut self, seq: u64) {
        self.send(json!({ "t": "ack", "seq": seq })).await;
    }

    async fn command(&mut self, seq: u64, key: &str, command: Value) {
        self.send(json!({
            "t": "command", "seq": seq, "idempotency_key": key,
            "issued_at": "2026-10-27T22:01:00.000Z", "command": command,
        }))
        .await;
    }

    /// `close{code}` followed by the WebSocket close with `4000 + n`.
    async fn close(&mut self, code: CloseCode) {
        let name = serde_json::to_value(code).unwrap();
        self.send(json!({ "t": "close", "code": name, "message": "test" }))
            .await;
        let _ = self
            .ws
            .send(Message::Close(Some(CloseFrame {
                code: WsCloseCode::from(code.ws_code()),
                reason: "".into(),
            })))
            .await;
    }

    /// Drops the TCP connection without a close.
    fn drop_socket(self) {
        drop(self);
    }

    /// Reads events `from..=to` and returns `(seq, event_id)` pairs.
    async fn events(&mut self, count: usize) -> Vec<(u64, String)> {
        let mut out = Vec::new();
        for _ in 0..count {
            let frame = self.recv().await;
            assert_eq!(frame["t"], "event", "{frame}");
            out.push((
                frame["seq"].as_u64().unwrap(),
                frame["event_id"].as_str().unwrap().to_owned(),
            ));
        }
        out
    }
}

// ---- rig ------------------------------------------------------------------------------------

struct ManualWall(AtomicU64);

impl ManualWall {
    fn new() -> Arc<Self> {
        Arc::new(Self(AtomicU64::new(1_800_000_000_000)))
    }

    fn jump(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl WallClock for ManualWall {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

struct Fixed(f64);

impl Jitter for Fixed {
    fn unit(&mut self) -> f64 {
        self.0
    }
}

struct Scripted(Vec<f64>, usize);

impl Jitter for Scripted {
    fn unit(&mut self) -> f64 {
        let value = self.0[self.1 % self.0.len()];
        self.1 += 1;
        value
    }
}

struct TestCredential {
    wipes: Arc<AtomicUsize>,
}

impl CredentialSource for TestCredential {
    async fn credential(&self) -> Result<String> {
        Ok(CREDENTIAL.to_owned())
    }

    fn wipe(&self) {
        self.wipes.fetch_add(1, Ordering::SeqCst);
    }
}

fn timing(base_ms: u64, cap_ms: u64) -> Timing {
    Timing {
        backoff_base: Duration::from_millis(base_ms),
        backoff_cap: Duration::from_millis(cap_ms),
        stable_after: Duration::from_secs(3600),
        welcome_timeout: Duration::from_secs(2),
    }
}

struct Rig {
    hub: FakeHub,
    store: Arc<Store>,
    sink: TunnelSink,
    commands: TunnelCommands,
    client: Option<TunnelClient<TestCredential>>,
    run: Option<JoinHandle<Result<Exit>>>,
    stop: Option<oneshot::Sender<()>>,
    wall: Arc<ManualWall>,
    wipes: Arc<AtomicUsize>,
    _dir: Option<TempDir>,
}

impl Rig {
    /// A client over a fresh store, not started yet.
    async fn new() -> Self {
        Self::with(timing(5, 40), 0.0).await
    }

    async fn with(timing: Timing, jitter: f64) -> Self {
        let dir = TempDir::new().unwrap();
        let hub = FakeHub::start().await;
        let mut rig = Self::build(hub, dir.path(), timing, jitter);
        rig._dir = Some(dir);
        rig
    }

    fn build(hub: FakeHub, state_dir: &Path, timing: Timing, jitter: f64) -> Self {
        let store = Arc::new(Store::open(state_dir).unwrap());
        let wall = ManualWall::new();
        let wipes = Arc::new(AtomicUsize::new(0));
        let config = TunnelConfig {
            hub_ws_base: hub.base(),
            machine_id: "mch_test".to_owned(),
            runner_version: "0.0.0-test".to_owned(),
            capabilities: Capabilities {
                host_mode: "local".to_owned(),
                harnesses: vec!["acp-mock".to_owned()],
                max_sessions: 1,
                spawn: vec!["same_dir".to_owned()],
                features: Vec::new(),
            },
            timing,
        };
        let (client, sink, commands) = tunnel(
            config,
            Arc::clone(&store),
            TestCredential {
                wipes: Arc::clone(&wipes),
            },
        );
        let client = client
            .with_wall_clock(wall.clone())
            .with_jitter(Box::new(Fixed(jitter)));
        Self {
            hub,
            store,
            sink,
            commands,
            client: Some(client),
            run: None,
            stop: None,
            wall,
            wipes,
            _dir: None,
        }
    }

    fn start(&mut self) {
        let (stop, stopped) = oneshot::channel::<()>();
        let client = self.client.take().expect("already started");
        self.run = Some(tokio::spawn(async move {
            client
                .run(async {
                    let _ = stopped.await;
                })
                .await
        }));
        self.stop = Some(stop);
    }

    async fn finished(&mut self) -> Result<Exit> {
        let run = self.run.take().expect("not started");
        tokio::time::timeout(WAIT, run)
            .await
            .expect("the client did not stop")
            .unwrap()
    }

    /// Connects, reads `hello` and answers a fresh-stream `welcome`.
    async fn connect(&mut self, heartbeat_ms: u64) -> HubConn {
        self.start();
        let mut conn = self.hub.next_conn().await;
        conn.hello().await;
        conn.welcome(false, "rt_1", 0, heartbeat_ms).await;
        conn
    }

    async fn emit(&self, delta: &str) {
        self.sink.send(vec![envelope("tk_1", delta)]).await.unwrap();
    }

    async fn wait_acked(&self, upto: u64) {
        wait_until(|| self.store.outbox_state().unwrap().acked_upto >= upto).await;
    }
}

async fn wait_until(mut condition: impl FnMut() -> bool) {
    let deadline = Instant::now() + WAIT;
    while !condition() {
        assert!(Instant::now() < deadline, "condition not reached in time");
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

fn envelope(task_id: &str, delta: &str) -> Envelope {
    serde_json::from_value(json!({
        "v": 1, "seq": 0, "thread_id": "th_1", "task_id": task_id, "run_id": "run_1",
        "ts": "2026-10-27T22:00:03.000Z",
        "event": { "type": "TextMessageContent", "message_id": "msg_a1", "delta": delta }
    }))
    .unwrap()
}

fn cancel(task: &str) -> Value {
    json!({ "kind": "cancel", "thread_id": "th_1", "task_id": task })
}

// ---- connection and hello -------------------------------------------------------------------

#[tokio::test]
async fn hello_comes_first_and_nothing_else_until_welcome() {
    let mut rig = Rig::new().await;
    // An event waits in the outbox before the tunnel even starts.
    rig.emit("queued").await;
    rig.start();
    let mut conn = rig.hub.next_conn().await;

    assert_eq!(conn.subprotocol.as_deref(), Some("sm.tunnel.v1"));
    assert_eq!(
        conn.authorization.as_deref(),
        Some(format!("Bearer {CREDENTIAL}").as_str())
    );
    assert_eq!(conn.path, "/v1/machines/mch_test/tunnel");
    assert!(!conn.path.contains(CREDENTIAL), "credential in the URL");

    let hello = conn.hello().await;
    assert_eq!(hello["proto"], 1);
    assert_eq!(hello["machine_id"], "mch_test");
    assert!(
        hello["resume_token"].is_null(),
        "first connect has no token"
    );
    assert_eq!(hello["last_down_seq"], 0);
    assert!(!hello.to_string().contains(CREDENTIAL));
    // Nothing else (no event, no ping) until `welcome`.
    assert!(conn.recv_within(Duration::from_millis(300)).await.is_none());

    conn.welcome(false, "rt_1", 0, 20_000).await;
    let events = conn.events(1).await;
    assert_eq!(events[0].0, 1);
}

#[tokio::test]
async fn the_resume_token_is_persisted_and_presented_on_reconnect() {
    let mut rig = Rig::new().await;
    let conn = rig.connect(20_000).await;
    wait_until(|| rig.store.resume_token().unwrap().as_deref() == Some("rt_1")).await;
    conn.drop_socket();

    let mut again = rig.hub.next_conn().await;
    let hello = again.hello().await;
    assert_eq!(hello["resume_token"], "rt_1");
    again.welcome(true, "rt_2", 0, 20_000).await;
    wait_until(|| rig.store.resume_token().unwrap().as_deref() == Some("rt_2")).await;
}

#[tokio::test]
async fn a_refused_upgrade_with_401_is_not_retried() {
    let dir = TempDir::new().unwrap();
    let hub = FakeHub::refusing(401).await;
    let mut rig = Rig::build(hub, dir.path(), timing(5, 40), 0.0);
    rig.start();
    let error = rig.finished().await.unwrap_err();
    assert!(error.to_string().contains("401"), "{error:#}");
    assert!(!error.to_string().contains(CREDENTIAL));
}

// ---- outbox, resume, fresh stream -----------------------------------------------------------

#[tokio::test]
async fn events_are_in_the_store_before_they_are_sent_and_pruned_on_ack() {
    let mut rig = Rig::new().await;
    rig.emit("one").await;
    rig.emit("two").await;
    // Durable before any connection exists.
    let state = rig.store.outbox_state().unwrap();
    assert_eq!(state.unacked.len(), 2);
    assert_eq!(state.unacked[0].body["t"], "event");

    let mut conn = rig.connect(20_000).await;
    let sent = conn.events(2).await;
    assert_eq!(sent.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2]);
    assert_eq!(rig.store.outbox_state().unwrap().unacked.len(), 2);
    conn.ack(2).await;
    rig.wait_acked(2).await;
    assert!(rig.store.outbox_state().unwrap().unacked.is_empty());
}

#[tokio::test]
async fn offline_queue_drains_in_order_after_welcome() {
    let mut rig = Rig::new().await;
    for i in 0..5 {
        rig.emit(&format!("d{i}")).await;
    }
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    conn.welcome(false, "rt_1", 0, 20_000).await;
    let events = conn.events(5).await;
    assert_eq!(
        events.iter().map(|e| e.0).collect::<Vec<_>>(),
        [1, 2, 3, 4, 5]
    );
    // The order of the deltas is the order they were emitted in.
    let stored = rig.store.outbox_state().unwrap().unacked;
    let deltas: Vec<_> = stored
        .iter()
        .map(|row| row.body["envelope"]["event"]["delta"].as_str().unwrap())
        .collect();
    assert_eq!(deltas, ["d0", "d1", "d2", "d3", "d4"]);
}

#[tokio::test]
async fn the_outbox_survives_a_runner_restart() {
    let dir = TempDir::new().unwrap();
    let hub = FakeHub::start().await;
    let before = {
        let rig = Rig::build(hub, dir.path(), timing(5, 40), 0.0);
        rig.emit("a").await;
        rig.emit("b").await;
        rig.store.outbox_state().unwrap().unacked
        // The first runner dies here without ever connecting.
    };
    assert_eq!(before.len(), 2);
    let hub = FakeHub::start().await;
    let mut rig = Rig::build(hub, dir.path(), timing(5, 40), 0.0);
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    conn.welcome(false, "rt_1", 0, 20_000).await;
    let events = conn.events(2).await;
    assert_eq!(events[0].1, before[0].event_id);
    assert_eq!(events[1].1, before[1].event_id);
}

#[tokio::test]
async fn resume_with_token_resends_only_what_the_hub_did_not_process() {
    let mut rig = Rig::new().await;
    let mut c1 = rig.connect(20_000).await;
    for d in ["e1", "e2", "e3"] {
        rig.emit(d).await;
    }
    let first = c1.events(3).await;
    c1.ack(2).await;
    rig.wait_acked(2).await;
    c1.drop_socket();
    // Emitted while the socket is down.
    rig.emit("e4").await;

    let mut c2 = rig.hub.next_conn().await;
    let hello = c2.hello().await;
    assert_eq!(hello["resume_token"], "rt_1");
    c2.welcome(true, "rt_2", 2, 20_000).await;
    let again = c2.events(2).await;
    // seq 3 again with its old event_id, then the new event: no loss, no duplicate.
    assert_eq!(again[0], first[2]);
    assert_eq!(again[1].0, 4);
    assert!(again[1].1 != first[0].1 && again[1].1 != first[1].1 && again[1].1 != first[2].1);
    assert!(c2.recv_within(Duration::from_millis(250)).await.is_none());
}

#[tokio::test]
async fn a_fresh_stream_renumbers_from_one_with_the_same_event_ids() {
    let mut rig = Rig::new().await;
    let mut c1 = rig.connect(20_000).await;
    for d in ["e1", "e2", "e3"] {
        rig.emit(d).await;
    }
    let first = c1.events(3).await;
    c1.ack(1).await;
    rig.wait_acked(1).await;
    c1.drop_socket();

    let mut c2 = rig.hub.next_conn().await;
    c2.hello().await;
    // The hub lost the token: a fresh stream.
    c2.welcome(false, "rt_2", 0, 20_000).await;
    let again = c2.events(2).await;
    assert_eq!(again[0], (1, first[1].1.clone()));
    assert_eq!(again[1], (2, first[2].1.clone()));
    assert_eq!(rig.store.outbox_state().unwrap().next_seq, 3);

    rig.emit("e4").await;
    let next = c2.events(1).await;
    assert_eq!(next[0].0, 3);
}

#[tokio::test]
async fn a_nack_counts_as_an_ack_and_fails_the_task_locally() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    rig.emit("bad").await;
    rig.emit("fine").await;
    conn.events(2).await;
    conn.send(json!({
        "t": "nack", "seq": 1, "code": "invalid_sequence",
        "message": "TextMessageContent before TextMessageStart",
    }))
    .await;
    // The nacked frame is gone from the outbox (it counts as acked) ...
    rig.wait_acked(1).await;
    let state = rig.store.outbox_state().unwrap();
    assert_eq!(state.unacked[0].seq, 2);
    // ... and the task fails with RunError{protocol} as a new frame.
    let failure = conn.recv().await;
    assert_eq!(failure["t"], "event");
    assert_eq!(failure["seq"], 3);
    assert_eq!(failure["envelope"]["task_id"], "tk_1");
    assert_eq!(failure["envelope"]["event"]["type"], "RunError");
    assert_eq!(failure["envelope"]["event"]["code"], "protocol");
    // The stream keeps going.
    conn.ack(3).await;
    rig.wait_acked(3).await;
}

#[tokio::test]
async fn a_command_result_failure_goes_up_with_its_key() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    rig.sink
        .command_result(CommandFailure {
            idempotency_key: "idk_9".to_owned(),
            code: protocol::tunnel::CommandErrorCode::Capacity,
            message: "32 sessions already running".to_owned(),
        })
        .await
        .unwrap();
    let frame = conn.recv().await;
    assert_eq!(frame["t"], "command_result");
    assert_eq!(frame["seq"], 1);
    assert_eq!(frame["idempotency_key"], "idk_9");
    assert_eq!(frame["ok"], false);
    assert_eq!(frame["code"], "capacity");
    assert!(frame["event_id"].as_str().unwrap().starts_with("evt_"));
}

// ---- down commands --------------------------------------------------------------------------

#[tokio::test]
async fn commands_reach_the_bridge_in_order_and_are_acked_after_processing() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    conn.command(1, "idk_1", cancel("tk_1")).await;
    conn.command(2, "idk_2", cancel("tk_2")).await;

    let first = rig.commands.next().await.unwrap();
    assert_eq!((first.seq, first.idempotency_key.as_str()), (1, "idk_1"));
    assert!(matches!(first.command, Command::Cancel { .. }));
    // Not acked while the bridge has not finished.
    assert!(conn.recv_within(Duration::from_millis(200)).await.is_none());

    rig.commands.processed(1, "idk_1").await;
    assert_eq!(conn.recv().await, json!({ "t": "ack", "seq": 1 }));
    let second = rig.commands.next().await.unwrap();
    assert_eq!(second.seq, 2);
    rig.commands.processed(2, "idk_2").await;
    assert_eq!(conn.recv().await, json!({ "t": "ack", "seq": 2 }));
    assert_eq!(rig.store.last_down_seq().unwrap(), 2);
}

#[tokio::test]
async fn a_repeated_idempotency_key_is_acked_but_not_applied_twice() {
    let mut rig = Rig::new().await;
    let mut c1 = rig.connect(20_000).await;
    c1.command(1, "idk_1", cancel("tk_1")).await;
    let first = rig.commands.next().await.unwrap();
    rig.commands
        .processed(first.seq, &first.idempotency_key)
        .await;
    assert_eq!(c1.recv().await["seq"], 1);
    c1.drop_socket();

    let mut c2 = rig.hub.next_conn().await;
    let hello = c2.hello().await;
    assert_eq!(hello["last_down_seq"], 1);
    // A fresh stream: the hub renumbers its queue from 1, so idk_1 comes back as seq 1.
    c2.welcome(false, "rt_2", 0, 20_000).await;
    c2.command(1, "idk_1", cancel("tk_1")).await;
    c2.command(2, "idk_2", cancel("tk_2")).await;
    let next = rig.commands.next().await.unwrap();
    assert_eq!((next.seq, next.idempotency_key.as_str()), (2, "idk_2"));
    rig.commands.processed(2, "idk_2").await;
    loop {
        let frame = c2.recv().await;
        assert_eq!(frame["t"], "ack");
        if frame["seq"] == 2 {
            break;
        }
    }
    let extra = tokio::time::timeout(Duration::from_millis(100), rig.commands.next()).await;
    assert!(extra.is_err(), "the duplicate key reached the bridge");
}

#[tokio::test]
async fn a_command_received_but_not_processed_is_redelivered_after_a_crash() {
    let dir = TempDir::new().unwrap();
    // First runner: receives the command, dies before the bridge reports it processed.
    {
        let hub = FakeHub::start().await;
        let mut rig = Rig::build(hub, dir.path(), timing(5, 40), 0.0);
        let mut conn = rig.connect(20_000).await;
        conn.command(1, "idk_1", cancel("tk_1")).await;
        let first = rig.commands.next().await.unwrap();
        assert_eq!(first.idempotency_key, "idk_1");
        // Received, fed to the bridge, but not processed: nothing acked, nothing committed.
        assert!(conn.recv_within(Duration::from_millis(200)).await.is_none());
        assert_eq!(rig.store.last_down_seq().unwrap(), 0);
        assert!(!rig.store.seen_key("idk_1").unwrap());
    }
    // Second runner on the same store: hello says 0, so the hub redelivers.
    let hub = FakeHub::start().await;
    let mut rig = Rig::build(hub, dir.path(), timing(5, 40), 0.0);
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    assert_eq!(conn.hello().await["last_down_seq"], 0);
    conn.welcome(true, "rt_2", 0, 20_000).await;
    conn.command(1, "idk_1", cancel("tk_1")).await;
    let again = rig.commands.next().await.unwrap();
    assert_eq!((again.seq, again.idempotency_key.as_str()), (1, "idk_1"));
    rig.commands.processed(1, "idk_1").await;
    assert_eq!(conn.recv().await, json!({ "t": "ack", "seq": 1 }));
    assert_eq!(rig.store.last_down_seq().unwrap(), 1);
    // A hub replay of the same key (renumbered stream) is acked but applied no more.
    conn.close(CloseCode::ServerRestart).await;
    let mut c3 = rig.hub.next_conn().await;
    assert_eq!(c3.hello().await["last_down_seq"], 1);
    c3.welcome(false, "rt_3", 0, 20_000).await;
    c3.command(1, "idk_1", cancel("tk_1")).await;
    assert_eq!(c3.recv().await, json!({ "t": "ack", "seq": 1 }));
    let extra = tokio::time::timeout(Duration::from_millis(100), rig.commands.next()).await;
    assert!(extra.is_err(), "the command was applied twice");
}

#[tokio::test]
async fn a_late_processed_from_before_a_fresh_stream_cannot_ack_a_new_command() {
    let mut rig = Rig::new().await;
    let mut c1 = rig.connect(20_000).await;
    c1.command(1, "idk_old", cancel("tk_1")).await;
    let old = rig.commands.next().await.unwrap();
    assert_eq!(old.idempotency_key, "idk_old");
    c1.drop_socket();

    // A fresh stream whose first command reuses seq 1.
    let mut c2 = rig.hub.next_conn().await;
    c2.hello().await;
    c2.welcome(false, "rt_2", 0, 20_000).await;
    c2.command(1, "idk_new", cancel("tk_2")).await;
    let new = rig.commands.next().await.unwrap();
    assert_eq!((new.seq, new.idempotency_key.as_str()), (1, "idk_new"));

    // The bridge finishes the OLD command now: seq 1 means the old numbering.
    rig.commands.processed(1, "idk_old").await;
    assert!(
        c2.recv_within(Duration::from_millis(300)).await.is_none(),
        "a late processed acked the new command"
    );
    assert_eq!(rig.store.last_down_seq().unwrap(), 0);
    assert!(!rig.store.seen_key("idk_new").unwrap());
    // The old command's effect is still recorded, without touching the cursor.
    assert!(rig.store.seen_key("idk_old").unwrap());

    rig.commands.processed(1, "idk_new").await;
    assert_eq!(c2.recv().await, json!({ "t": "ack", "seq": 1 }));
    assert_eq!(rig.store.last_down_seq().unwrap(), 1);
}

#[tokio::test]
async fn processed_reports_out_of_order_ack_and_commit_the_right_commands() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    for (seq, key) in [(1, "idk_1"), (2, "idk_2"), (3, "idk_3")] {
        conn.command(seq, key, cancel("tk")).await;
        let got = rig.commands.next().await.unwrap();
        assert_eq!((got.seq, got.idempotency_key.as_str()), (seq, key));
    }
    // The bridge finishes 3, then 2: nothing may be committed or acked past the unprocessed 1.
    rig.commands.processed(3, "idk_3").await;
    rig.commands.processed(2, "idk_2").await;
    assert!(conn.recv_within(Duration::from_millis(200)).await.is_none());
    assert_eq!(rig.store.last_down_seq().unwrap(), 0);
    rig.commands.processed(1, "idk_1").await;
    let mut acked = 0;
    while acked < 3 {
        let frame = conn.recv().await;
        assert_eq!(frame["t"], "ack");
        acked = frame["seq"].as_u64().unwrap();
    }
    assert_eq!(rig.store.last_down_seq().unwrap(), 3);
    for key in ["idk_1", "idk_2", "idk_3"] {
        assert!(rig.store.seen_key(key).unwrap(), "{key}");
    }
}

#[tokio::test]
async fn welcome_max_inflight_caps_unacked_up_frames() {
    let mut rig = Rig::new().await;
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    conn.welcome_with_window(false, "rt_1", 2).await;
    for d in ["e1", "e2", "e3", "e4", "e5"] {
        rig.emit(d).await;
    }
    let first = conn.events(2).await;
    assert_eq!(first.iter().map(|e| e.0).collect::<Vec<_>>(), [1, 2]);
    assert!(
        conn.recv_within(Duration::from_millis(300)).await.is_none(),
        "more than max_inflight frames were in flight"
    );
    conn.ack(1).await;
    assert_eq!(conn.events(1).await[0].0, 3);
    assert!(conn.recv_within(Duration::from_millis(200)).await.is_none());
    conn.ack(3).await;
    let rest = conn.events(2).await;
    assert_eq!(rest.iter().map(|e| e.0).collect::<Vec<_>>(), [4, 5]);
}

#[tokio::test]
async fn a_gap_in_the_down_stream_closes_with_protocol_error_and_reconnects() {
    let mut rig = Rig::new().await;
    let mut c1 = rig.connect(20_000).await;
    c1.command(2, "idk_2", cancel("tk_1")).await;
    let close = c1.recv().await;
    assert_eq!(close["t"], "close");
    assert_eq!(close["code"], "protocol_error");
    let mut c2 = rig.hub.next_conn().await;
    assert_eq!(c2.hello().await["last_down_seq"], 0);
}

#[tokio::test]
async fn an_unknown_command_kind_is_answered_unsupported() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    conn.command(1, "idk_7", json!({ "kind": "teleport", "where": "moon" }))
        .await;
    let result = conn.recv().await;
    assert_eq!(result["t"], "command_result");
    assert_eq!(result["idempotency_key"], "idk_7");
    assert_eq!(result["code"], "unsupported");
    assert_eq!(result["ok"], false);
    // The down frame itself is acked.
    assert_eq!(conn.recv().await, json!({ "t": "ack", "seq": 1 }));
}

// ---- close codes ----------------------------------------------------------------------------

async fn no_reconnect(code: CloseCode) {
    let mut rig = Rig::new().await;
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    conn.welcome(false, "rt_1", 0, 20_000).await;
    conn.close(code).await;
    let exit = rig.finished().await.unwrap();
    assert_eq!(exit, Exit::Closed(code));
    assert!(
        rig.hub
            .try_next_conn(Duration::from_millis(300))
            .await
            .is_none(),
        "{code:?} must not reconnect"
    );
    // The sink refuses output once the client stopped.
    assert!(rig.sink.send(vec![envelope("tk_1", "late")]).await.is_err());
    let wiped = usize::from(code == CloseCode::AuthRevoked);
    assert_eq!(rig.wipes.load(Ordering::SeqCst), wiped);
}

async fn reconnects(code: CloseCode) {
    let mut rig = Rig::new().await;
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    conn.welcome(false, "rt_1", 0, 20_000).await;
    conn.close(code).await;
    let mut again = rig.hub.next_conn().await;
    again.hello().await;
    assert!(rig.run.as_ref().is_some_and(|run| !run.is_finished()));
}

#[tokio::test]
async fn close_normal_does_not_reconnect() {
    no_reconnect(CloseCode::Normal).await;
}

#[tokio::test]
async fn close_superseded_does_not_reconnect() {
    no_reconnect(CloseCode::Superseded).await;
}

#[tokio::test]
async fn close_auth_invalid_does_not_reconnect() {
    no_reconnect(CloseCode::AuthInvalid).await;
}

#[tokio::test]
async fn close_auth_revoked_does_not_reconnect_and_wipes_the_credential() {
    no_reconnect(CloseCode::AuthRevoked).await;
}

#[tokio::test]
async fn close_version_unsupported_does_not_reconnect() {
    no_reconnect(CloseCode::VersionUnsupported).await;
}

#[tokio::test]
async fn close_protocol_error_reconnects() {
    reconnects(CloseCode::ProtocolError).await;
}

#[tokio::test]
async fn close_heartbeat_timeout_reconnects() {
    reconnects(CloseCode::HeartbeatTimeout).await;
}

#[tokio::test]
async fn close_server_restart_reconnects() {
    reconnects(CloseCode::ServerRestart).await;
}

#[tokio::test]
async fn a_lost_socket_without_a_close_reconnects() {
    let mut rig = Rig::new().await;
    let conn = rig.connect(20_000).await;
    conn.drop_socket();
    let mut again = rig.hub.next_conn().await;
    again.hello().await;
}

#[tokio::test]
async fn shutdown_sends_close_normal() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(20_000).await;
    rig.stop.take().unwrap().send(()).unwrap();
    let close = conn.recv().await;
    assert_eq!(close["t"], "close");
    assert_eq!(close["code"], "normal");
    assert_eq!(rig.finished().await.unwrap(), Exit::Shutdown);
}

// ---- thread_open and presence ---------------------------------------------------------------

fn workspace() -> Workspace {
    Workspace {
        path: "/Users/me/src/app".to_owned(),
        branch: Some("main".to_owned()),
        spawn: None,
    }
}

#[tokio::test]
async fn thread_open_goes_out_before_the_first_event_of_its_thread() {
    let mut rig = Rig::new().await;
    // Queued while the tunnel is down, as a local session starts before it connects.
    rig.sink
        .thread_open("th_1", "fix flaky test", "claude", &workspace())
        .unwrap();
    rig.emit("first").await;
    let mut conn = rig.connect(20_000).await;

    let open = conn.recv().await;
    assert_eq!(open["t"], "thread_open");
    assert_eq!(open["seq"], 1);
    assert_eq!(open["thread_id"], "th_1");
    assert_eq!(open["title"], "fix flaky test");
    assert_eq!(open["agent"], "claude");
    assert_eq!(
        open["workspace"],
        json!({ "path": "/Users/me/src/app", "branch": "main" })
    );
    assert!(open["event_id"].as_str().unwrap().starts_with("evt_"));
    let events = conn.events(1).await;
    assert_eq!(events[0].0, 2, "the event follows its thread_open");

    conn.ack(2).await;
    rig.wait_acked(2).await;
    assert!(rig.store.outbox_state().unwrap().unacked.is_empty());
}

#[tokio::test]
async fn thread_open_is_resent_first_after_a_resumed_reconnect() {
    let mut rig = Rig::new().await;
    rig.sink
        .thread_open("th_1", "fix flaky test", "claude", &workspace())
        .unwrap();
    rig.emit("first").await;
    let mut conn = rig.connect(20_000).await;
    let open = conn.recv().await;
    let event = conn.recv().await;
    assert_eq!(event["t"], "event");

    // The hub took both frames but acked neither, then the socket dropped.
    conn.drop_socket();
    let mut again = rig.hub.next_conn().await;
    again.hello().await;
    again.welcome(true, "rt_2", 0, 20_000).await;
    assert_eq!(again.recv().await, open, "thread_open resent unchanged");
    assert_eq!(again.recv().await, event, "event resent after it");

    again.ack(2).await;
    rig.wait_acked(2).await;
    assert!(rig.store.outbox_state().unwrap().unacked.is_empty());
}

#[tokio::test]
async fn thread_open_is_renumbered_first_after_a_fresh_stream() {
    let mut rig = Rig::new().await;
    rig.sink
        .thread_open("th_1", "fix flaky test", "claude", &workspace())
        .unwrap();
    rig.emit("first").await;
    let mut conn = rig.connect(20_000).await;
    conn.recv().await;
    conn.events(1).await;

    conn.drop_socket();
    let mut again = rig.hub.next_conn().await;
    again.hello().await;
    again.welcome(false, "rt_2", 0, 20_000).await;
    let open = again.recv().await;
    assert_eq!(open["t"], "thread_open");
    assert_eq!(open["seq"], 1, "a fresh stream numbers from one");
    assert_eq!(open["thread_id"], "th_1");
    let events = again.events(1).await;
    assert_eq!(events[0].0, 2);
}

#[tokio::test]
async fn the_sleep_hint_sends_presence_asleep_on_the_live_connection() {
    let mut rig = Rig::new().await;
    let hint = rig.client.as_ref().unwrap().sleep_hint();
    let mut conn = rig.connect(20_000).await;

    assert!(
        hint.asleep().await,
        "the hint was not written to the socket"
    );
    // Presence is not sequenced and not in the outbox (§3.7).
    assert_eq!(
        conn.recv().await,
        json!({ "t": "presence", "state": "asleep", "reason": "os_sleep" })
    );
    assert!(rig.store.outbox_state().unwrap().unacked.is_empty());
}

#[tokio::test]
async fn the_sleep_hint_gives_up_without_a_connection_so_the_sleep_is_not_held() {
    let rig = Rig::new().await;
    let hint = rig.client.as_ref().unwrap().sleep_hint();
    let started = Instant::now();
    assert!(!hint.asleep().await);
    assert!(started.elapsed() < Duration::from_secs(3));
}

#[tokio::test]
async fn shutdown_after_the_sleep_hint_sends_close_normal_and_never_reconnects() {
    let mut rig = Rig::new().await;
    let hint = rig.client.as_ref().unwrap().sleep_hint();
    let mut conn = rig.connect(20_000).await;
    assert!(hint.asleep().await);
    assert_eq!(conn.recv().await["t"], "presence");

    rig.stop.take().unwrap().send(()).unwrap();
    let close = conn.recv().await;
    assert_eq!(close["t"], "close");
    assert_eq!(close["code"], "normal");
    assert_eq!(rig.finished().await.unwrap(), Exit::Shutdown);
    assert!(
        rig.hub
            .try_next_conn(Duration::from_millis(300))
            .await
            .is_none(),
        "close{{normal}} must not reconnect"
    );
}

// ---- backoff --------------------------------------------------------------------------------

#[test]
fn backoff_ceiling_doubles_from_500ms_up_to_the_30s_cap() {
    let mut backoff = Backoff::new(
        Duration::from_millis(500),
        Duration::from_secs(30),
        Box::new(Fixed(0.999_999)),
    );
    let ceilings: Vec<u64> = (0..9)
        .map(|_| {
            let ceiling = backoff.ceiling();
            backoff.next_delay();
            ceiling.as_millis() as u64
        })
        .collect();
    assert_eq!(
        ceilings,
        [500, 1000, 2000, 4000, 8000, 16_000, 30_000, 30_000, 30_000]
    );
}

#[test]
fn backoff_uses_full_jitter_within_zero_and_the_ceiling() {
    let mut scripted = Backoff::new(
        Duration::from_millis(500),
        Duration::from_secs(30),
        Box::new(Scripted(vec![0.0, 0.5, 0.999], 0)),
    );
    assert_eq!(scripted.next_delay(), Duration::ZERO);
    assert_eq!(scripted.next_delay(), Duration::from_millis(500));
    let third = scripted.next_delay();
    assert!(third < Duration::from_millis(2000) && third > Duration::from_millis(1990));

    // Real randomness stays inside [0, ceiling).
    let mut random = Backoff::new(
        Duration::from_millis(500),
        Duration::from_secs(30),
        Box::new(runner::tunnel::RandomJitter),
    );
    let mut seen_nonzero = false;
    for _ in 0..200 {
        let ceiling = random.ceiling();
        let delay = random.next_delay();
        assert!(delay <= ceiling);
        seen_nonzero |= delay > Duration::ZERO;
        if random.attempts() > 8 {
            random.reset();
        }
    }
    assert!(seen_nonzero);
}

#[test]
fn backoff_reset_returns_to_the_minimum() {
    let mut backoff = Backoff::new(
        Duration::from_millis(500),
        Duration::from_secs(30),
        Box::new(Fixed(0.999_999)),
    );
    for _ in 0..5 {
        backoff.next_delay();
    }
    assert!(backoff.ceiling() > Duration::from_secs(8));
    backoff.reset();
    assert_eq!(backoff.ceiling(), Duration::from_millis(500));
}

#[tokio::test]
async fn reconnect_delays_grow_with_each_failed_attempt() {
    // Jitter 0.99: the delay is just under the ceiling, so gaps are bounded below by it.
    let mut rig = Rig::with(timing(40, 160), 0.99).await;
    rig.start();
    let mut accepted = Vec::new();
    for _ in 0..5 {
        let mut conn = rig.hub.next_conn().await;
        accepted.push(conn.accepted_at);
        conn.hello().await;
        conn.close(CloseCode::ProtocolError).await;
    }
    let gaps: Vec<u64> = accepted
        .windows(2)
        .map(|w| (w[1] - w[0]).as_millis() as u64)
        .collect();
    // Ceilings 40, 80, 160, 160 (cap), each scaled by 0.99.
    for (gap, ceiling) in gaps.iter().zip([40u64, 80, 160, 160]) {
        assert!(
            *gap >= ceiling * 9 / 10,
            "gap {gap} ms under ceiling {ceiling} ms: {gaps:?}"
        );
        assert!(
            *gap < ceiling + 1500,
            "gap {gap} ms way over the ceiling {ceiling}: {gaps:?}"
        );
    }
    assert!(gaps[2] > gaps[0], "{gaps:?}");
}

#[tokio::test]
async fn server_restart_reconnects_from_the_minimum_delay() {
    let mut rig = Rig::with(timing(100, 1600), 0.99).await;
    rig.start();
    for _ in 0..3 {
        let mut conn = rig.hub.next_conn().await;
        conn.hello().await;
        conn.close(CloseCode::ProtocolError).await;
    }
    // Without a reset the next ceiling would be 800 ms (delay ~792 ms).
    let mut conn = rig.hub.next_conn().await;
    conn.hello().await;
    let restarted = Instant::now();
    conn.close(CloseCode::ServerRestart).await;
    let again = rig.hub.next_conn().await;
    let gap = again.accepted_at - restarted;
    assert!(gap < Duration::from_millis(600), "{gap:?}");
}

// ---- heartbeat, sleep/wake ------------------------------------------------------------------

#[tokio::test]
async fn pings_go_out_every_heartbeat_as_the_literal_string() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(80).await;
    let mut times = Vec::new();
    while times.len() < 4 {
        let raw = conn.raw_within(WAIT).await.expect("no ping");
        assert_eq!(raw, r#"{"t":"ping"}"#);
        times.push(Instant::now());
        conn.send(json!({ "t": "pong" })).await;
    }
    for pair in times.windows(2) {
        let gap = pair[1] - pair[0];
        assert!(gap >= Duration::from_millis(60), "pings too close: {gap:?}");
        assert!(
            gap < Duration::from_millis(800),
            "pings too far apart: {gap:?}"
        );
    }
}

#[tokio::test]
async fn three_silent_heartbeats_close_with_heartbeat_timeout_and_reconnect() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(50).await;
    // The hub never answers: pings, then the runner gives up.
    let mut pings = 0;
    let close = loop {
        let raw = conn.raw_within(WAIT).await.expect("no close");
        if raw == r#"{"t":"ping"}"# {
            pings += 1;
        } else {
            break serde_json::from_str::<Value>(&raw).unwrap();
        }
    };
    assert!((2..=4).contains(&pings), "{pings} pings before the timeout");
    assert_eq!(close["t"], "close");
    assert_eq!(close["code"], "heartbeat_timeout");
    let mut again = rig.hub.next_conn().await;
    again.hello().await;
}

#[tokio::test]
async fn a_wall_clock_jump_reconnects_at_once_without_a_heartbeat_timeout() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(100).await;
    // Wait until the runner is in its main loop (it answered `welcome` with a ping).
    assert_eq!(conn.raw_within(WAIT).await.unwrap(), r#"{"t":"ping"}"#);
    conn.send(json!({ "t": "pong" })).await;
    // The OS slept for an hour: wall clock +1 h, the monotonic clock did not move.
    rig.wall.jump(3_600_000);
    let mut again = rig.hub.next_conn().await;
    let hello = again.hello().await;
    assert_eq!(hello["resume_token"], "rt_1", "it resumes after waking");
    // The old socket was dropped on wake detection, not closed for silence.
    while let Some(frame) = conn.raw_within(Duration::from_millis(300)).await {
        assert!(!frame.contains("heartbeat_timeout"), "{frame}");
    }
}

#[tokio::test]
async fn a_steady_wall_clock_keeps_the_connection() {
    let mut rig = Rig::new().await;
    let mut conn = rig.connect(60).await;
    for _ in 0..5 {
        let raw = conn.raw_within(WAIT).await.expect("no ping");
        assert_eq!(raw, r#"{"t":"ping"}"#);
        conn.send(json!({ "t": "pong" })).await;
    }
    assert!(
        rig.hub
            .try_next_conn(Duration::from_millis(100))
            .await
            .is_none()
    );
}

// ---- outbound only --------------------------------------------------------------------------

/// Local ports of TCP sockets in LISTEN state that this process owns (Linux).
fn listening_ports() -> Vec<u16> {
    let mut inodes = Vec::new();
    for entry in std::fs::read_dir("/proc/self/fd").unwrap().flatten() {
        if let Ok(target) = std::fs::read_link(entry.path())
            && let Some(inode) = target
                .to_string_lossy()
                .strip_prefix("socket:[")
                .and_then(|rest| rest.strip_suffix(']'))
        {
            inodes.push(inode.to_owned());
        }
    }
    let mut ports = Vec::new();
    for table in ["/proc/net/tcp", "/proc/net/tcp6"] {
        let Ok(text) = std::fs::read_to_string(table) else {
            continue;
        };
        for line in text.lines().skip(1) {
            let fields: Vec<&str> = line.split_whitespace().collect();
            // fields: sl local_address rem_address st ... inode
            if fields.len() > 9 && fields[3] == "0A" && inodes.iter().any(|i| i == fields[9]) {
                let port = fields[1].rsplit(':').next().unwrap();
                ports.push(u16::from_str_radix(port, 16).unwrap());
            }
        }
    }
    ports.sort_unstable();
    ports
}

fn assert_only_hubs_listen(own_hub: u16) {
    let hubs = HUB_PORTS.lock().unwrap().clone();
    let ports = listening_ports();
    assert!(ports.contains(&own_hub), "the fake hub should be listening");
    let strangers: Vec<_> = ports.iter().filter(|p| !hubs.contains(p)).collect();
    assert!(
        strangers.is_empty(),
        "listening sockets besides the fake hubs: {strangers:?}"
    );
}

#[tokio::test]
async fn the_runner_opens_no_listening_sockets() {
    let mut rig = Rig::new().await;
    let hub_port = rig.hub.addr.port();
    let mut conn = rig.connect(20_000).await;
    rig.emit("x").await;
    conn.events(1).await;
    conn.command(1, "idk_1", cancel("tk_1")).await;
    rig.commands.next().await.unwrap();
    // The only listeners in this process are the fake hubs of the parallel tests.
    assert_only_hubs_listen(hub_port);
    // Also while reconnecting.
    conn.drop_socket();
    rig.hub.next_conn().await;
    assert_only_hubs_listen(hub_port);
}

// ---- one store shared by the bridge and the tunnel client -----------------------------------

/// An acp-mock behind a tee that records the methods the bridge sent. With `stall`, the agent
/// never gets `session/new`, so the bridge stays inside applying its first command.
struct TeedAgent {
    methods: Arc<Mutex<Vec<String>>>,
    stalled: mpsc::UnboundedReceiver<()>,
    tasks: Vec<JoinHandle<()>>,
}

impl TeedAgent {
    fn start(
        stall: bool,
    ) -> (
        Self,
        impl tokio::io::AsyncRead + Send + Unpin + 'static,
        impl tokio::io::AsyncWrite + Send + Unpin + 'static,
    ) {
        use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};
        let script =
            acp_mock::Script::from_value(&json!({ "turns": [{ "steps": [{ "text": "hi" }] }] }))
                .unwrap();
        let (client_io, proxy_client) = tokio::io::duplex(1 << 16);
        let (proxy_agent, agent_io) = tokio::io::duplex(1 << 16);
        let (agent_r, agent_w) = tokio::io::split(agent_io);
        let mock = tokio::spawn(async move {
            let _ = acp_mock::run(script, agent_r, agent_w).await;
        });
        let (client_up, mut to_client) = tokio::io::split(proxy_client);
        let (mut from_agent, mut to_agent) = tokio::io::split(proxy_agent);
        let methods = Arc::new(Mutex::new(Vec::new()));
        let record = Arc::clone(&methods);
        let (stalled_tx, stalled) = mpsc::unbounded_channel();
        let up = tokio::spawn(async move {
            let mut lines = BufReader::new(client_up).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                let method = serde_json::from_str::<Value>(&line)
                    .ok()
                    .and_then(|m| m["method"].as_str().map(str::to_owned));
                if let Some(method) = &method {
                    record.lock().unwrap().push(method.clone());
                }
                if stall && method.as_deref() == Some("session/new") {
                    let _ = stalled_tx.send(());
                    continue;
                }
                if to_agent
                    .write_all(format!("{line}\n").as_bytes())
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
        let down = tokio::spawn(async move {
            let _ = tokio::io::copy(&mut from_agent, &mut to_client).await;
        });
        let (client_r, client_w) = tokio::io::split(client_io);
        (
            Self {
                methods,
                stalled,
                tasks: vec![mock, up, down],
            },
            client_r,
            client_w,
        )
    }

    fn prompts(&self) -> usize {
        let methods = self.methods.lock().unwrap();
        methods.iter().filter(|m| *m == "session/prompt").count()
    }

    /// The process dies: everything it ran stops.
    fn kill(&self) {
        for task in &self.tasks {
            task.abort();
        }
    }
}

fn prompt_command() -> Value {
    json!({
        "kind": "prompt", "thread_id": "th_1", "task_id": "tk_1", "message_id": "msg_1",
        "parts": [{ "type": "text", "text": "go" }],
        "author": { "kind": "user", "id": "usr_1" },
    })
}

/// Bridge, tunnel client and fake hub as the real runner wires them: one Store for both.
fn start_bridge(
    rig: &mut Rig,
    workdir: &Path,
    stall: bool,
) -> (TeedAgent, JoinHandle<()>, mpsc::UnboundedReceiver<()>) {
    let (mut agent, reader, writer) = TeedAgent::start(stall);
    let bridge = Bridge::connect(
        Config::new("mch_test", "agt_mock", workdir),
        reader,
        writer,
        Arc::clone(&rig.store),
        Arc::new(SequentialGenerator::new()),
        rig.sink.clone(),
        std::mem::replace(&mut rig.commands, dead_commands()),
    );
    let run = tokio::spawn(async move {
        let _ = bridge.run().await;
    });
    let stalled = std::mem::replace(&mut agent.stalled, mpsc::unbounded_channel().1);
    (agent, run, stalled)
}

/// A `TunnelCommands` nobody feeds, to leave behind in the rig.
fn dead_commands() -> TunnelCommands {
    let dir = TempDir::new().unwrap();
    let store = Arc::new(Store::open(dir.path()).unwrap());
    let config = TunnelConfig {
        hub_ws_base: "ws://127.0.0.1:1".to_owned(),
        machine_id: "mch_dead".to_owned(),
        runner_version: "0".to_owned(),
        capabilities: Capabilities {
            host_mode: "local".to_owned(),
            harnesses: Vec::new(),
            max_sessions: 1,
            spawn: Vec::new(),
            features: Vec::new(),
        },
        timing: timing(5, 40),
    };
    let (_client, _sink, commands) = tunnel(
        config,
        store,
        TestCredential {
            wipes: Arc::new(AtomicUsize::new(0)),
        },
    );
    commands
}

#[tokio::test]
async fn a_command_received_but_not_applied_is_applied_exactly_once_after_a_crash() {
    let state = TempDir::new().unwrap();
    let work = TempDir::new().unwrap();
    // First process: the bridge has the command and is inside applying it (the agent never
    // answers `session/new`) when everything dies.
    {
        let hub = FakeHub::start().await;
        let mut rig = Rig::build(hub, state.path(), timing(5, 40), 0.0);
        let (agent, bridge, mut stalled) = start_bridge(&mut rig, work.path(), true);
        let mut conn = rig.connect(20_000).await;
        conn.command(1, "idk_1", prompt_command()).await;
        tokio::time::timeout(WAIT, stalled.recv())
            .await
            .expect("the bridge never started applying the command");
        assert_eq!(agent.prompts(), 0);
        // Received and not applied: nothing may be committed or acked.
        assert_eq!(rig.store.last_down_seq().unwrap(), 0);
        assert!(!rig.store.seen_key("idk_1").unwrap());
        bridge.abort();
        agent.kill();
        rig.run.take().unwrap().abort();
        let _ = bridge.await;
    }
    // Second process, same store: hello.last_down_seq is still 0, so the hub redelivers.
    let hub = FakeHub::start().await;
    let mut rig = Rig::build(hub, state.path(), timing(5, 40), 0.0);
    let (agent, bridge, _) = start_bridge(&mut rig, work.path(), false);
    rig.start();
    let mut conn = rig.hub.next_conn().await;
    assert_eq!(conn.hello().await["last_down_seq"], 0);
    conn.welcome(true, "rt_2", 0, 20_000).await;
    conn.command(1, "idk_1", prompt_command()).await;
    loop {
        let frame = conn.recv().await;
        if frame["t"] == "ack" {
            assert_eq!(frame["seq"], 1);
            break;
        }
    }
    assert_eq!(rig.store.last_down_seq().unwrap(), 1);
    assert!(rig.store.seen_key("idk_1").unwrap());
    assert_eq!(agent.prompts(), 1, "the prompt reached the agent once");
    // A hub replay of the same key is acked and not applied again.
    conn.close(CloseCode::ServerRestart).await;
    let mut c3 = rig.hub.next_conn().await;
    assert_eq!(c3.hello().await["last_down_seq"], 1);
    c3.welcome(false, "rt_3", 0, 20_000).await;
    c3.command(1, "idk_1", prompt_command()).await;
    loop {
        let frame = c3.recv().await;
        if frame["t"] == "ack" {
            break;
        }
    }
    tokio::time::sleep(Duration::from_millis(200)).await;
    assert_eq!(agent.prompts(), 1, "the prompt was applied twice");
    bridge.abort();
    agent.kill();
}
