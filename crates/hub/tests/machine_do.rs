//! M2B-002 live tests of the Machine Durable Object against a running hub
//! (`wrangler dev --local`).
//!
//! Needs `HUB_URL` (for example `http://127.0.0.1:8787`) and a hub started with
//! `DEV_FIXTURES=1` (`crates/hub/.dev.vars` does that for `wrangler dev`) and the D1 migrations
//! applied. Run it through `bash scripts/hub-test.sh machine_do`, which builds the Worker,
//! applies the migrations, starts wrangler and sets `HUB_URL`. Without `HUB_URL` every test
//! fails with a message saying so; nothing passes vacuously. The `live` feature (required by
//! `[[test]] machine_do` in Cargo.toml) keeps this file out of a plain `cargo test -p hub`.
//!
//! The runner side is a hand-written tunnel client on `std::net` plus the protocol crate's own
//! `Sender` / `Receiver`, so the tests also show what a runner must do (tunnel.md §4).
//! Every test uses its own machine and thread ids (one Durable Object each).

use std::io::{Read, Write};
use std::net::{Shutdown, TcpStream};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::tunnel::{Receiver, Sender, Verdict};
use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);
const POLL_TIMEOUT: Duration = Duration::from_secs(15);

// ---- plain HTTP -------------------------------------------------------------------------

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_str(&self.body)
            .unwrap_or_else(|error| panic!("body is not JSON ({error}): {:?}", self.body))
    }
}

fn authority() -> String {
    let url = match std::env::var("HUB_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_owned(),
        _ => panic!(
            "HUB_URL is not set: this test needs a running hub. \
             Run it with `bash scripts/hub-test.sh machine_do`."
        ),
    };
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

fn connect() -> TcpStream {
    let authority = authority();
    let stream = TcpStream::connect(&authority)
        .unwrap_or_else(|error| panic!("cannot connect to HUB_URL {authority}: {error}"));
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    stream
}

/// One request with `Connection: close`; `auth` adds `Authorization: Bearer dev`.
fn http(
    method: &str,
    path: &str,
    auth: bool,
    idempotency_key: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let mut stream = connect();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        authority()
    );
    if auth {
        request.push_str("Authorization: Bearer dev\r\n");
    }
    if let Some(key) = idempotency_key {
        request.push_str(&format!("Idempotency-Key: {key}\r\n"));
    }
    let payload = body.map(Value::to_string).unwrap_or_default();
    if body.is_some() {
        request.push_str("Content-Type: application/json\r\n");
    }
    request.push_str(&format!(
        "Content-Length: {}\r\n\r\n{payload}",
        payload.len()
    ));
    stream.write_all(request.as_bytes()).expect("send request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse_reply(&String::from_utf8_lossy(&raw))
}

fn parse_reply(text: &str) -> Reply {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
    let status_line = head.split("\r\n").next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Reply { status, body }
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let Some(data) = after.get(..size) else {
            break;
        };
        out.push_str(data);
        rest = after.get(size + 2..).unwrap_or_default();
    }
    out
}

// ---- a minimal WebSocket client ---------------------------------------------------------

/// What the server sent on a WebSocket.
#[derive(Debug, Clone, PartialEq)]
enum Msg {
    Json(Value),
    /// A WebSocket close frame with its code.
    Close(Option<u16>),
}

#[derive(Debug)]
struct Ws {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    /// Opens a WebSocket with the given extra headers. `Err` carries the response head when the
    /// server did not answer 101.
    fn open(path: &str, headers: &[(&str, &str)]) -> Result<Self, String> {
        let mut stream = connect();
        let mut request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n",
            authority()
        );
        for (name, value) in headers {
            request.push_str(&format!("{name}: {value}\r\n"));
        }
        request.push_str("\r\n");
        stream.write_all(request.as_bytes()).expect("send upgrade");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                other => panic!("connection ended during the upgrade ({other:?}): {head:?}"),
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        if !head.starts_with("HTTP/1.1 101") {
            return Err(head);
        }
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    /// A subscription to a thread (turn-event.md §6).
    fn subscribe(thread: &str) -> Self {
        Self::open(
            &format!("/v1/threads/{thread}/subscribe"),
            &[("Authorization", "Bearer dev")],
        )
        .unwrap_or_else(|head| panic!("subscribe refused: {head}"))
    }

    /// Sends one masked text frame.
    fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![0x81u8];
        match payload.len() {
            len if len < 126 => frame.push(0x80 | len as u8),
            len if len <= usize::from(u16::MAX) => {
                frame.push(0x80 | 126);
                frame.extend_from_slice(&(len as u16).to_be_bytes());
            }
            len => {
                frame.push(0x80 | 127);
                frame.extend_from_slice(&(len as u64).to_be_bytes());
            }
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        // The peer may already have closed the socket; the caller finds out when it reads.
        let _ = self.stream.write_all(&frame);
    }

    /// The next message, or `None` when nothing arrives within `wait`.
    fn recv_within(&mut self, wait: Duration) -> Option<Msg> {
        let deadline = Instant::now() + wait;
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        let text = String::from_utf8_lossy(&payload).into_owned();
                        let value = serde_json::from_str(&text)
                            .unwrap_or_else(|error| panic!("frame is not JSON ({error}): {text}"));
                        return Some(Msg::Json(value));
                    }
                    0x8 => {
                        let code = (payload.len() >= 2)
                            .then(|| u16::from_be_bytes([payload[0], payload[1]]));
                        return Some(Msg::Close(code));
                    }
                    _ => continue, // ping / pong / binary: ignore
                }
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return None;
            }
            self.stream
                .set_read_timeout(Some(left.max(Duration::from_millis(1))))
                .expect("set read timeout");
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => return Some(Msg::Close(None)),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(error)
                    if matches!(
                        error.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    return None;
                }
                Err(_) => return Some(Msg::Close(None)),
            }
        }
    }

    fn recv(&mut self) -> Msg {
        self.recv_within(READ_TIMEOUT)
            .unwrap_or_else(|| panic!("no WebSocket message in {READ_TIMEOUT:?}"))
    }

    /// The next JSON message; panics on a close.
    fn json(&mut self) -> Value {
        match self.recv() {
            Msg::Json(value) => value,
            Msg::Close(code) => panic!("the server closed the WebSocket ({code:?})"),
        }
    }

    /// Expects `close{code}` and then the WebSocket close frame `ws_code`.
    fn expect_close(&mut self, code: &str, ws_code: u16) {
        let frame = self.json();
        assert_eq!(frame["t"], "close", "expected a close frame, got {frame}");
        assert_eq!(frame["code"], code, "close frame: {frame}");
        assert_eq!(self.recv(), Msg::Close(Some(ws_code)));
    }

    /// Drops the TCP connection without a close handshake (a network loss).
    fn drop_abruptly(self) {
        let _ = self.stream.shutdown(Shutdown::Both);
    }
}

/// `(opcode, payload, bytes used)` of the first complete frame in `buf`.
fn parse_frame(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0f;
    let masked = buf[1] & 0x80 != 0;
    let mut len = usize::from(buf[1] & 0x7f);
    let mut offset = 2;
    if len == 126 {
        let bytes = buf.get(2..4)?;
        len = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        offset = 4;
    } else if len == 127 {
        let bytes: [u8; 8] = buf.get(2..10)?.try_into().ok()?;
        len = usize::try_from(u64::from_be_bytes(bytes)).ok()?;
        offset = 10;
    }
    if masked {
        offset += 4;
    }
    let payload = buf.get(offset..offset + len)?;
    Some((opcode, payload.to_vec(), offset + len))
}

// ---- the runner side --------------------------------------------------------------------

/// A runner as tunnel.md §4 describes it: an up `Sender` (its outbox), a down `Receiver`, the
/// last resume token.
struct Runner {
    ws: Ws,
    up: Sender<Value>,
    down: Receiver,
    token: Option<String>,
    welcome: Value,
    /// Command frames received, in order, duplicates dropped.
    commands: Vec<Value>,
    nacks: Vec<Value>,
    /// `interrupt_answer_result` frames received, in order.
    results: Vec<Value>,
    /// The thread the test opened on this runner (for `interrupt_answer`).
    thread: String,
    next_event: u64,
}

/// What survives a restart of the runner process.
struct Persisted {
    up: Sender<Value>,
    token: Option<String>,
    last_down: u64,
    next_event: u64,
}

fn hello(machine: &str, token: Option<&str>, last_down_seq: u64) -> Value {
    json!({
        "t": "hello",
        "proto": 1,
        "machine_id": machine,
        "runner_version": "0.0.0-test",
        "resume_token": token,
        "last_down_seq": last_down_seq,
        "capabilities": {
            "host_mode": "local",
            "harnesses": ["claude"],
            "max_sessions": 4,
            "spawn": ["same_dir"],
            "features": [],
        },
    })
}

fn tunnel_path(machine: &str) -> String {
    format!("/v1/machines/{machine}/tunnel")
}

/// Opens the tunnel socket with a credential (subprotocol `sm.tunnel.v1`).
fn open_tunnel(machine: &str, credential: Option<&str>) -> Result<Ws, String> {
    let bearer = credential.map(|c| format!("Bearer {c}"));
    let mut headers = vec![("Sec-WebSocket-Protocol", "sm.tunnel.v1")];
    if let Some(bearer) = &bearer {
        headers.push(("Authorization", bearer));
    }
    Ws::open(&tunnel_path(machine), &headers)
}

impl Runner {
    /// A first connection: no token, empty outbox.
    fn first(machine: &str, credential: &str) -> Self {
        Self::connect(
            machine,
            credential,
            Persisted {
                up: Sender::new(256),
                token: None,
                last_down: 0,
                next_event: 0,
            },
        )
    }

    /// `hello`, `welcome`, then the resend of the outbox (tunnel.md §3.2).
    fn connect(machine: &str, credential: &str, saved: Persisted) -> Self {
        let mut ws = open_tunnel(machine, Some(credential))
            .unwrap_or_else(|head| panic!("tunnel refused: {head}"));
        ws.send_text(&hello(machine, saved.token.as_deref(), saved.last_down).to_string());
        let welcome = ws.json();
        assert_eq!(welcome["t"], "welcome", "first frame: {welcome}");
        let resumed = welcome["resumed"].as_bool().expect("resumed");
        let last_up_seq = welcome["last_up_seq"].as_u64().expect("last_up_seq");
        let mut up = saved.up;
        up.on_welcome(resumed, last_up_seq).expect("welcome cursor");
        let mut runner = Self {
            ws,
            up,
            // A resumed stream continues numbering; a fresh one restarts it at 1 (§4.4).
            down: if resumed {
                Receiver::resume(saved.last_down)
            } else {
                Receiver::fresh()
            },
            token: welcome["resume_token"].as_str().map(str::to_owned),
            welcome,
            commands: Vec::new(),
            nacks: Vec::new(),
            results: Vec::new(),
            thread: String::new(),
            next_event: saved.next_event,
        };
        runner.flush();
        runner
    }

    fn resumed(&self) -> bool {
        self.welcome["resumed"] == true
    }

    /// Puts every frame the window allows on the wire.
    fn flush(&mut self) {
        while let Some((seq, mut body)) = self.up.next_to_send() {
            body["seq"] = json!(seq);
            self.ws.send_text(&body.to_string());
        }
    }

    /// Queues an `event` frame (a fresh `event_id`) and sends it.
    fn event(&mut self, envelope: Value) -> String {
        self.next_event += 1;
        let event_id = format!("evt_{:x}_{}", now_ms(), self.next_event);
        self.up
            .enqueue(json!({"t": "event", "event_id": event_id, "envelope": envelope}));
        self.flush();
        event_id
    }

    /// Queues any other up frame (`thread_open`, `command_result`).
    fn up_frame(&mut self, frame: Value) {
        self.up.enqueue(frame);
        self.flush();
    }

    fn open_thread(&mut self, thread: &str) {
        self.next_event += 1;
        self.up_frame(json!({
            "t": "thread_open",
            "event_id": format!("evt_open_{thread}_{}", self.next_event),
            "thread_id": thread,
            "title": "tunnel test",
            "agent": "claude",
            "workspace": {"path": "/tmp/work", "branch": "main"},
        }));
    }

    /// Handles one frame from the hub.
    fn handle(&mut self, frame: &Value) {
        match frame["t"].as_str() {
            Some("ack") => self
                .up
                .on_ack(frame["seq"].as_u64().expect("seq"))
                .expect("ack"),
            Some("nack") => {
                self.up
                    .on_nack(frame["seq"].as_u64().expect("seq"))
                    .expect("nack");
                self.nacks.push(frame.clone());
            }
            Some("command") => {
                let seq = frame["seq"].as_u64().expect("seq");
                if let Ok(Verdict::Process) = self.down.on_frame(seq) {
                    self.commands.push(frame.clone());
                }
            }
            Some("interrupt_answer_result") => self.results.push(frame.clone()),
            Some("pong") | None => {}
            other => panic!("unexpected frame {other:?}: {frame}"),
        }
    }

    /// Reads frames until `done` holds.
    fn read_until(&mut self, what: &str, done: impl Fn(&Self) -> bool) {
        let deadline = Instant::now() + POLL_TIMEOUT;
        while !done(self) {
            let left = deadline.saturating_duration_since(Instant::now());
            assert!(!left.is_zero(), "timed out waiting for {what}");
            match self.ws.recv_within(left) {
                Some(Msg::Json(frame)) => self.handle(&frame),
                Some(Msg::Close(code)) => {
                    panic!("the hub closed the tunnel ({code:?}) while waiting for {what}")
                }
                None => {}
            }
        }
    }

    /// Reads until every up frame is acked (or nacked).
    fn read_acks(&mut self) {
        self.read_until("acks of the up stream", |r| r.up.is_empty());
    }

    /// Reads until `n` commands were received in total.
    fn read_commands(&mut self, n: usize) {
        self.read_until(&format!("{n} commands"), |r| r.commands.len() >= n);
    }

    /// Reads until `n` `interrupt_answer_result` frames were received in total.
    fn read_results(&mut self, n: usize) {
        self.read_until(&format!("{n} interrupt answer results"), |r| {
            r.results.len() >= n
        });
    }

    /// Acks the down stream up to what was processed.
    fn ack_down(&mut self) {
        let seq = self.down.last();
        self.ws
            .send_text(&json!({"t": "ack", "seq": seq}).to_string());
    }

    /// The command `kind`s received so far.
    fn kinds(&self) -> Vec<String> {
        self.commands
            .iter()
            .map(|c| c["command"]["kind"].as_str().unwrap_or_default().to_owned())
            .collect()
    }

    fn seqs(&self) -> Vec<u64> {
        self.commands
            .iter()
            .filter_map(|c| c["seq"].as_u64())
            .collect()
    }

    /// Drops the connection like a network loss and returns what a restart would keep.
    fn crash(self, keep_down: Option<u64>) -> Persisted {
        let last_down = keep_down.unwrap_or_else(|| self.down.last());
        let saved = Persisted {
            up: self.up,
            token: self.token,
            last_down,
            next_event: self.next_event,
        };
        self.ws.drop_abruptly();
        saved
    }
}

// ---- helpers ----------------------------------------------------------------------------

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn unique(prefix: &str, name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}_{name}_{}_{nanos:x}", std::process::id())
}

/// A machine for the Machine DO named `machine`, through the DEV_FIXTURES credential route.
fn dev_credential(machine: &str) -> String {
    let reply = http(
        "POST",
        &format!("/v1/dev/machines/{machine}/credential"),
        true,
        None,
        None,
    );
    assert_eq!(reply.status, 201, "dev credential: {}", reply.body);
    reply.json()["credential"]
        .as_str()
        .expect("credential")
        .to_owned()
}

fn machine_state(machine: &str) -> Value {
    let reply = http(
        "GET",
        &format!("/v1/dev/machines/{machine}/state"),
        true,
        None,
        None,
    );
    assert_eq!(reply.status, 200, "machine state: {}", reply.body);
    reply.json()
}

/// Polls the Machine DO's state until `done` holds.
fn wait_state(machine: &str, what: &str, done: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let state = machine_state(machine);
        if done(&state) {
            return state;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}; state: {state}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// Registers the Thread DO's host (the init hook that thread creation will call).
fn init_thread(thread: &str, machine: &str) {
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/init"),
        true,
        None,
        Some(&json!({
            "machine_id": machine,
            "agent": "claude",
            "title": "machine test",
            "workspace": {"path": "/tmp/work"},
            "permission_mode": "default",
        })),
    );
    assert_eq!(reply.status, 200, "thread init: {}", reply.body);
}

fn prompt(thread: &str, task: &str, key: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        true,
        Some(key),
        Some(&json!({"task_id": task, "parts": [{"type": "text", "text": "do it"}]})),
    )
}

/// A prompt command as a Thread DO would push it.
fn pushed_prompt(thread: &str, source: &str, task: &str) -> Value {
    json!({
        "source_id": source,
        "idempotency_key": format!("idk_{source}"),
        "issued_at": format_timestamp(now_ms()),
        "command": {
            "kind": "prompt",
            "thread_id": thread,
            "task_id": task,
            "message_id": format!("msg_{task}"),
            "parts": [{"type": "text", "text": "go"}],
            "author": {"kind": "user", "id": "usr_dev", "device_id": "dev_dev"},
        },
    })
}

fn push_commands(machine: &str, thread: &str, commands: Vec<Value>) {
    let reply = http(
        "POST",
        &format!("/v1/dev/machines/{machine}/commands"),
        true,
        None,
        Some(
            &json!({"thread_id": thread, "host": "thread", "snapshot": false, "commands": commands}),
        ),
    );
    assert_eq!(reply.status, 200, "push commands: {}", reply.body);
}

fn envelope(thread: &str, task: Option<&str>, run: Option<&str>, event: Value) -> Value {
    json!({
        "v": 1,
        "seq": 0,
        "thread_id": thread,
        "task_id": task,
        "run_id": run,
        "ts": format_timestamp(now_ms()),
        "event": event,
    })
}

fn user_message(thread: &str) -> Value {
    envelope(
        thread,
        Some("tk_1"),
        None,
        json!({
            "type": "UserMessage",
            "message_id": "msg_u1",
            "parts": [{"type": "text", "text": "hi"}],
            "author": {"kind": "user", "id": "usr_dev", "device_id": "dev_dev"},
        }),
    )
}

fn run_event(thread: &str, event: Value) -> Value {
    envelope(thread, Some("tk_1"), Some("run_1"), event)
}

fn started() -> Value {
    json!({"type": "RunStarted", "reason": "prompt"})
}

fn text_start() -> Value {
    json!({"type": "TextMessageStart", "message_id": "m1", "role": "assistant"})
}

fn text_content(delta: &str) -> Value {
    json!({"type": "TextMessageContent", "message_id": "m1", "delta": delta})
}

fn text_end() -> Value {
    json!({"type": "TextMessageEnd", "message_id": "m1"})
}

fn finished() -> Value {
    json!({"type": "RunFinished", "outcome": {"type": "success", "stop_reason": "end_turn"}})
}

/// The next `Presence` envelope on a subscription.
fn next_presence(subscriber: &mut Ws) -> Value {
    loop {
        let message = subscriber.json();
        if message["type"] == "event" && type_of(&message["envelope"]) == "Presence" {
            return message["envelope"].clone();
        }
    }
}

/// The envelopes of a thread, read through a fresh subscription (`?after=0`).
fn thread_events(thread: &str) -> Vec<Value> {
    let mut ws = Ws::subscribe(thread);
    let mut events = Vec::new();
    loop {
        let message = ws.json();
        match message["type"].as_str() {
            Some("event") => events.push(message["envelope"].clone()),
            Some("live") => return events,
            _ => {}
        }
    }
}

fn type_of(envelope: &Value) -> &str {
    envelope["event"]["type"].as_str().unwrap_or_default()
}

/// The event types of a thread without `Presence` and `StateSnapshot` (the hub's own noise).
fn run_types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .map(type_of)
        .filter(|t| !matches!(*t, "Presence" | "StateSnapshot"))
        .collect()
}

fn deltas(events: &[Value]) -> Vec<String> {
    events
        .iter()
        .filter(|e| type_of(e) == "TextMessageContent")
        .map(|e| e["event"]["delta"].as_str().unwrap_or_default().to_owned())
        .collect()
}

fn assert_gap_free(events: &[Value]) {
    let seqs: Vec<u64> = events.iter().filter_map(|e| e["seq"].as_u64()).collect();
    let expected: Vec<u64> = (1..=seqs.len() as u64).collect();
    assert_eq!(seqs, expected, "envelope seq is gap-free: {seqs:?}");
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn the_tunnel_needs_this_machines_credential() {
    let machine = unique("mch", "auth");
    let other = unique("mch", "auth_other");
    let credential = dev_credential(&machine);
    let other_credential = dev_credential(&other);

    // No credential: refused before the upgrade. Never in the URL either.
    let refused = open_tunnel(&machine, None).expect_err("no credential");
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    // The subprotocol is required (tunnel.md §2).
    let refused = Ws::open(
        &tunnel_path(&machine),
        &[("Authorization", &format!("Bearer {credential}"))],
    )
    .expect_err("no subprotocol");
    assert!(refused.starts_with("HTTP/1.1 426"), "{refused}");

    // A credential that is not this machine's, and another machine's credential: auth_invalid.
    for wrong in [format!("smm_{}", "a".repeat(64)), other_credential.clone()] {
        let mut ws = open_tunnel(&machine, Some(&wrong)).expect("accepted, then closed");
        ws.expect_close("auth_invalid", 4002);
    }

    // The right credential, but a hello for another machine: the identity is the credential's.
    let mut ws = open_tunnel(&machine, Some(&credential)).expect("tunnel");
    ws.send_text(&hello(&other, None, 0).to_string());
    ws.expect_close("auth_invalid", 4002);

    // An unsupported protocol version.
    let mut ws = open_tunnel(&machine, Some(&credential)).expect("tunnel");
    let mut bad = hello(&machine, None, 0);
    bad["proto"] = json!(2);
    ws.send_text(&bad.to_string());
    ws.expect_close("version_unsupported", 4004);

    // The right credential and hello: welcome.
    let runner = Runner::first(&machine, &credential);
    assert_eq!(runner.welcome["resumed"], false);
    assert_eq!(runner.welcome["last_up_seq"], 0);
    assert_eq!(runner.welcome["heartbeat_ms"], 20000);
    assert!(
        runner
            .token
            .as_deref()
            .is_some_and(|t| t.starts_with("rt_"))
    );
}

#[test]
fn revoking_a_machine_closes_its_tunnel_and_it_cannot_reconnect() {
    // The admin API needs a session; `Bearer dev` is an owner of the dev org.
    let denied = http(
        "POST",
        "/v1/machines",
        false,
        None,
        Some(&json!({"name": "x"})),
    );
    assert_eq!(denied.status, 401, "{}", denied.body);
    let invalid = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({"name": "", "kind": "vm"})),
    );
    assert_eq!(invalid.status, 400, "{}", invalid.body);

    let name = unique("laptop", "revoke");
    let created = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({"name": name, "kind": "local"})),
    );
    assert_eq!(created.status, 201, "create: {}", created.body);
    let created = created.json();
    let machine = created["machine"]["id"].as_str().expect("id").to_owned();
    let credential = created["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    assert!(credential.starts_with("smm_") && credential.len() == 68);
    assert!(created["machine"].get("credential_hash").is_none());

    // It shows up in the list, offline, and the credential is not in the answer.
    let listed = http("GET", "/v1/machines", true, None, None);
    assert_eq!(listed.status, 200, "{}", listed.body);
    assert!(!listed.body.contains(&credential));
    let row = listed_row(&machine).expect("machine in the list");
    assert_eq!(row["presence"]["state"], "offline");
    assert!(row["presence"]["since"].is_string(), "{row}");
    assert_eq!(row["capabilities"], Value::Null, "no hello yet");

    // It connects with the issued credential and is online.
    let mut runner = Runner::first(&machine, &credential);
    wait_state(&machine, "online", |s| s["presence"] == "online");

    // Revoking closes the live tunnel with auth_revoked.
    let revoked = http(
        "DELETE",
        &format!("/v1/machines/{machine}"),
        true,
        None,
        None,
    );
    assert_eq!(revoked.status, 200, "revoke: {}", revoked.body);
    assert!(revoked.json()["revoked_at"].is_string());
    runner.ws.expect_close("auth_revoked", 4003);

    // ... and it cannot come back: the same credential is told it was revoked.
    let mut again = open_tunnel(&machine, Some(&credential)).expect("accepted, then closed");
    again.expect_close("auth_revoked", 4003);
    // A wrong credential still sees auth_invalid, not the revoked state.
    let mut wrong =
        open_tunnel(&machine, Some(&format!("smm_{}", "b".repeat(64)))).expect("accepted");
    wrong.expect_close("auth_invalid", 4002);

    // Revoking twice is fine, an unknown machine is 404, the list shows the revocation.
    assert_eq!(
        http(
            "DELETE",
            &format!("/v1/machines/{machine}"),
            true,
            None,
            None
        )
        .status,
        200
    );
    assert_eq!(
        http("DELETE", "/v1/machines/mch_nope", true, None, None).status,
        404
    );
    assert!(
        listed_row(&machine).is_none(),
        "a revoked machine is left out of the list"
    );

    // Both mutations are audited, and the credential is nowhere in the log.
    let audit = http("GET", "/v1/dev/audit?limit=1000", true, None, None);
    assert_eq!(audit.status, 200, "{}", audit.body);
    assert!(!audit.body.contains(&credential));
    let actions: Vec<(String, String)> = audit.json()["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .filter(|e| e["target_id"] == machine.as_str())
        .map(|e| {
            (
                e["action"].as_str().unwrap_or_default().to_owned(),
                e["actor_user_id"].as_str().unwrap_or_default().to_owned(),
            )
        })
        .collect();
    assert!(
        actions.contains(&("machine.create".to_owned(), "usr_dev".to_owned())),
        "{actions:?}"
    );
    assert!(
        actions.contains(&("machine.revoke".to_owned(), "usr_dev".to_owned())),
        "{actions:?}"
    );
}

#[test]
fn a_new_connection_supersedes_the_old_one() {
    let machine = unique("mch", "super");
    let credential = dev_credential(&machine);
    let mut first = Runner::first(&machine, &credential);
    // The heartbeat is answered by the runtime itself (setWebSocketAutoResponse).
    first.ws.send_text(r#"{"t":"ping"}"#);
    assert_eq!(first.ws.json(), json!({"t": "pong"}));

    let mut second = Runner::first(&machine, &credential);
    first.ws.expect_close("superseded", 4001);
    second.ws.send_text(r#"{"t":"ping"}"#);
    assert_eq!(second.ws.json(), json!({"t": "pong"}));
    // The runtime reports when it last answered a ping for the hub, which is how the watchdog
    // (3 x heartbeat_ms of silence = offline) sees a runner whose pings never wake the object.
    let state = machine_state(&machine);
    let answered = state["auto_response_ms"].as_u64().unwrap_or_else(|| {
        panic!(
            "the runtime reports no auto-answered ping: {}",
            state["auto_response_ms"]
        )
    });
    assert!(
        answered + 15_000 > now_ms(),
        "auto-response timestamp {answered} is stale"
    );

    let state = wait_state(&machine, "the second connection", |s| {
        s["connected"] == true
    });
    assert_eq!(state["connected"], true);
    assert_eq!(
        state["presence"], "online",
        "supersession does not flap presence"
    );
}

#[test]
fn events_are_validated_appended_to_the_thread_and_acked_and_bad_frames_are_nacked() {
    let machine = unique("mch", "events");
    let thread = unique("th", "events");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);

    runner.open_thread(&thread); // seq 1: registers the thread, calls the Thread DO init hook
    // seq 2: an event for a thread this machine does not host.
    runner.event(run_event("th_not_mine", started()));
    runner.event(user_message(&thread)); // 3
    runner.event(run_event(&thread, started())); // 4
    // seq 5: valid shape, but TextMessageContent before TextMessageStart (turn-event.md §5).
    runner.event(run_event(&thread, text_content("early")));
    // seq 6: hosted, but not an envelope.
    runner.event(json!({"thread_id": thread, "v": 1}));
    runner.event(run_event(&thread, text_start())); // 7
    runner.event(run_event(&thread, text_content("hello"))); // 8
    runner.event(run_event(&thread, text_end())); // 9
    runner.event(run_event(&thread, finished())); // 10
    runner.read_acks();

    // A nack counts as an ack: nothing is left in the outbox, and the codes are the spec's.
    let nacks: Vec<(u64, String)> = runner
        .nacks
        .iter()
        .map(|n| {
            (
                n["seq"].as_u64().unwrap_or(0),
                n["code"].as_str().unwrap_or("").to_owned(),
            )
        })
        .collect();
    assert_eq!(
        nacks,
        vec![
            (2, "forbidden".to_owned()),
            (5, "invalid_sequence".to_owned()),
            (6, "invalid_envelope".to_owned()),
        ]
    );

    let events = thread_events(&thread);
    assert_eq!(
        run_types(&events),
        [
            "UserMessage",
            "RunStarted",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageEnd",
            "RunFinished"
        ]
    );
    assert_gap_free(&events);
    assert_eq!(deltas(&events), ["hello"]);
    // The thread knows its host from thread_open.
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(info["machine_id"], machine.as_str());
    assert_eq!(info["title"], "tunnel test");

    // A frame that is not a frame closes the tunnel with protocol_error.
    runner.ws.send_text("this is not json");
    runner.ws.expect_close("protocol_error", 4005);
}

#[test]
fn commands_queued_offline_are_delivered_in_order_after_welcome() {
    let machine = unique("mch", "queue");
    let thread = unique("th", "queue");
    let credential = dev_credential(&machine);
    init_thread(&thread, &machine);

    // The machine is offline. A client prompt goes through the Thread DO, which pushes the
    // command to the Machine DO; two more commands arrive the same way.
    let reply = prompt(&thread, "tk_1", "idk_queue_1");
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);
    push_commands(
        &machine,
        &thread,
        vec![
            json!({
                "source_id": "900",
                "idempotency_key": "idk_queue_2",
                "issued_at": format_timestamp(now_ms()),
                "command": {
                    "kind": "approval", "thread_id": thread, "task_id": "tk_1",
                    "interrupt_id": "int_1",
                    "outcome": {"type": "selected", "option_id": "allow"},
                    "resolved_by": {"kind": "user", "id": "usr_dev", "device_id": "dev_dev"},
                },
            }),
            json!({
                "source_id": "901",
                "idempotency_key": "idk_queue_3",
                "issued_at": format_timestamp(now_ms()),
                "command": {"kind": "cancel", "thread_id": thread, "task_id": "tk_1"},
            }),
        ],
    );
    let state = machine_state(&machine);
    assert_eq!(state["outbox_len"], 3, "queued while offline: {state}");
    assert_eq!(state["connected"], false);
    assert_eq!(state["hosted"][0]["thread_id"], thread.as_str());

    // After welcome, in order, with gap-free seq.
    let mut runner = Runner::first(&machine, &credential);
    runner.read_commands(3);
    assert_eq!(runner.kinds(), ["prompt", "approval", "cancel"]);
    assert_eq!(runner.seqs(), [1, 2, 3]);
    assert_eq!(runner.commands[0]["idempotency_key"], "idk_queue_1");
    assert_eq!(runner.commands[0]["command"]["task_id"], "tk_1");
    assert_eq!(runner.commands[2]["idempotency_key"], "idk_queue_3");

    // The ack empties the hub's outbox.
    runner.ack_down();
    let state = wait_state(&machine, "the ack", |s| s["outbox_len"] == 0);
    assert_eq!(state["down_base"], 3);

    // Online, a command goes out at once.
    push_commands(
        &machine,
        &thread,
        vec![pushed_prompt(&thread, "902", "tk_2")],
    );
    runner.read_commands(4);
    assert_eq!(runner.seqs(), [1, 2, 3, 4]);
}

#[test]
fn a_dropped_socket_resumes_with_the_token_without_loss_or_duplicates() {
    let machine = unique("mch", "resume");
    let thread = unique("th", "resume");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    assert!(!runner.resumed());
    let first_token = runner.token.clone();

    // Mid-turn: the runner has told the hub about the turn so far.
    runner.open_thread(&thread);
    runner.event(user_message(&thread));
    runner.event(run_event(&thread, started()));
    runner.event(run_event(&thread, text_start()));
    runner.event(run_event(&thread, text_content("a")));
    runner.read_acks();

    // Two commands go down; the runner processes only the first before the socket drops.
    push_commands(
        &machine,
        &thread,
        vec![
            pushed_prompt(&thread, "1", "tk_a"),
            pushed_prompt(&thread, "2", "tk_b"),
        ],
    );
    runner.read_commands(2);
    assert_eq!(runner.seqs(), [1, 2]);

    // One more event goes on the wire, and the network dies before the ack is read.
    runner.event(run_event(&thread, text_content("b")));
    let saved = runner.crash(Some(1));
    wait_state(&machine, "the loss to be noticed", |s| {
        s["connected"] == false
    });

    // Reconnect with the token: resumed, both streams continue (tunnel.md §4.3).
    let mut runner = Runner::connect(&machine, &credential, saved);
    assert!(runner.resumed(), "welcome: {}", runner.welcome);
    assert_ne!(runner.token, first_token, "the token rotates");
    let last_up = runner.welcome["last_up_seq"].as_u64().expect("last_up_seq");
    assert!(
        last_up == 5 || last_up == 6,
        "hub processed up to e5 or e6, got {last_up}"
    );
    // Only the command the runner did not process is sent again, with its old seq.
    runner.read_commands(1);
    assert_eq!(runner.seqs(), [2]);
    assert_eq!(runner.commands[0]["command"]["task_id"], "tk_b");

    // The turn goes on. Nothing is lost and nothing is appended twice.
    runner.event(run_event(&thread, text_end()));
    runner.event(run_event(&thread, finished()));
    runner.read_acks();
    let events = thread_events(&thread);
    assert_eq!(
        run_types(&events),
        [
            "UserMessage",
            "RunStarted",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageContent",
            "TextMessageEnd",
            "RunFinished"
        ]
    );
    assert_eq!(deltas(&events), ["a", "b"]);
    assert_gap_free(&events);
    // The loss showed as presence offline, then online again.
    let presence: Vec<&str> = events
        .iter()
        .filter(|e| type_of(e) == "Presence")
        .filter_map(|e| e["event"]["state"].as_str())
        .collect();
    assert_eq!(presence, ["offline", "online"]);
}

#[test]
fn a_fresh_stream_without_a_token_dedupes_events_by_event_id() {
    let machine = unique("mch", "fresh");
    let thread = unique("th", "fresh");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);

    runner.open_thread(&thread);
    runner.event(user_message(&thread));
    runner.event(run_event(&thread, started()));
    runner.event(run_event(&thread, text_start()));
    runner.event(run_event(&thread, text_content("x")));
    // The hub processes all five frames, but the runner never reads the acks.
    wait_state(&machine, "five frames processed", |s| s["up_last"] == 5);
    // A command goes down on the old stream; it is never acked.
    push_commands(&machine, &thread, vec![pushed_prompt(&thread, "1", "tk_a")]);
    wait_state(&machine, "the command on the wire", |s| s["down_sent"] == 1);
    let first_key = json!("idk_1");
    let saved = runner.crash(None);
    wait_state(&machine, "the loss to be noticed", |s| {
        s["connected"] == false
    });

    // The runner restarted from an old backup: no token, but its outbox still holds the five
    // frames (same event_ids) and it adds one more.
    let mut restarted = Persisted {
        token: None,
        last_down: 0,
        ..saved
    };
    restarted.up.enqueue(json!({
        "t": "event",
        "event_id": "evt_new_end",
        "envelope": run_event(&thread, text_end()),
    }));
    let mut runner = Runner::connect(&machine, &credential, restarted);
    assert!(
        !runner.resumed(),
        "no token: a fresh stream, welcome: {}",
        runner.welcome
    );
    assert_eq!(runner.welcome["last_up_seq"], 0);
    runner.read_acks();

    // The five old events were not appended again; the new one was.
    let events = thread_events(&thread);
    assert_eq!(
        run_types(&events),
        [
            "UserMessage",
            "RunStarted",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageEnd"
        ]
    );
    assert_eq!(deltas(&events), ["x"]);
    assert_gap_free(&events);
    // Both sides restarted their numbering at 1 (tunnel.md §4.4).
    let state = wait_state(&machine, "six frames on the new stream", |s| {
        s["up_last"] == 6
    });
    assert_eq!(state["stream_epoch"], 2);
    // The hub renumbered its unacked command from 1; the runner dedupes it by idempotency key.
    runner.read_commands(1);
    assert_eq!(runner.seqs(), [1]);
    assert_eq!(runner.commands[0]["idempotency_key"], first_key);
}

#[test]
fn presence_reaches_subscribed_threads_and_d1() {
    let name = unique("box", "presence");
    let created = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({"name": name, "kind": "sandbox"})),
    )
    .json();
    let machine = created["machine"]["id"].as_str().expect("id").to_owned();
    let credential = created["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    let thread = unique("th", "presence");

    let mut runner = Runner::first(&machine, &credential);
    runner.open_thread(&thread);
    runner.read_acks();

    let mut subscriber = Ws::subscribe(&thread);
    loop {
        if subscriber.json()["type"] == "live" {
            break;
        }
    }
    // An asleep hint, then the socket is lost: asleep.
    runner
        .ws
        .send_text(&json!({"t": "presence", "state": "asleep", "reason": "os_sleep"}).to_string());
    std::thread::sleep(Duration::from_millis(300));
    let saved = runner.crash(None);
    let asleep = next_presence(&mut subscriber);
    assert_eq!(asleep["event"]["state"], "asleep");
    assert_eq!(asleep["event"]["machine_id"], machine.as_str());
    assert!(asleep["event"]["since"].is_string());
    assert_eq!(asleep["task_id"], Value::Null);

    // Reconnect: online.
    let runner = Runner::connect(&machine, &credential, saved);
    let online = next_presence(&mut subscriber);
    assert_eq!(online["event"]["state"], "online");
    wait_state(&machine, "online", |s| s["presence"] == "online");

    // Lost without a hint: offline. D1 has the same story.
    let _saved = runner.crash(None);
    let offline = next_presence(&mut subscriber);
    assert_eq!(offline["event"]["state"], "offline");
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let row = listed_row(&machine).expect("machine in the list");
        if row["presence"]["state"] == "offline" {
            assert!(row["presence"]["since"].is_string());
            break;
        }
        assert!(
            Instant::now() < deadline,
            "D1 presence did not follow: {row}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

/// The `GET /v1/machines` row of `machine`, if it is listed.
fn listed_row(machine: &str) -> Option<Value> {
    let reply = http("GET", "/v1/machines", true, None, None);
    assert_eq!(reply.status, 200, "{}", reply.body);
    reply.json()["machines"]
        .as_array()
        .and_then(|rows| rows.iter().find(|m| m["machine_id"] == machine).cloned())
}

#[test]
fn the_machine_list_is_the_client_api_shape_and_shows_the_hello_capabilities() {
    let name = unique("box", "list");
    let created = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({"name": name, "kind": "sandbox"})),
    )
    .json();
    let machine = created["machine"]["id"].as_str().expect("id").to_owned();
    let credential = created["credential"]
        .as_str()
        .expect("credential")
        .to_owned();

    let row = listed_row(&machine).expect("in the list before the first hello");
    assert_eq!(row["name"], name.as_str());
    assert_eq!(row["kind"], "sandbox");
    let mut keys: Vec<&str> = row
        .as_object()
        .expect("a row is an object")
        .keys()
        .map(String::as_str)
        .collect();
    keys.sort_unstable();
    assert_eq!(
        keys,
        ["capabilities", "kind", "machine_id", "name", "presence"],
        "{row}"
    );

    // The runner says hello: online, and its capabilities as the three §5.1 fields.
    let _runner = Runner::first(&machine, &credential);
    let deadline = Instant::now() + POLL_TIMEOUT;
    loop {
        let row = listed_row(&machine).expect("machine in the list");
        if row["presence"]["state"] == "online" && row["capabilities"] != Value::Null {
            assert_eq!(
                row["capabilities"],
                json!({"harnesses": ["claude"], "spawn": ["same_dir"], "max_sessions": 4}),
                "{row}"
            );
            break;
        }
        assert!(
            Instant::now() < deadline,
            "no online row with capabilities: {row}"
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

#[test]
fn a_failed_command_result_fails_the_task() {
    let machine = unique("mch", "fail");
    let thread = unique("th", "fail");
    let credential = dev_credential(&machine);
    init_thread(&thread, &machine);
    let mut runner = Runner::first(&machine, &credential);

    // A client prompt: the Thread DO appends UserMessage and queues the command; its push
    // reaches the connected runner at once, under the client's idempotency key.
    let reply = prompt(&thread, "tk_1", "idk_fail_1");
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);
    runner.read_commands(1);
    assert_eq!(runner.commands[0]["idempotency_key"], "idk_fail_1");
    runner.ack_down();

    // The runner cannot start it.
    runner.up_frame(json!({
        "t": "command_result",
        "event_id": "evt_fail_1",
        "idempotency_key": "idk_fail_1",
        "ok": false,
        "code": "capacity",
        "message": "32 sessions already running",
    }));
    runner.read_acks();

    let deadline = Instant::now() + POLL_TIMEOUT;
    let events = loop {
        let events = thread_events(&thread);
        if events.iter().any(|e| type_of(e) == "RunError") {
            break events;
        }
        assert!(
            Instant::now() < deadline,
            "the task was not failed: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
    };
    assert_eq!(
        run_types(&events),
        ["UserMessage", "RunStarted", "RunError"]
    );
    let error = events
        .iter()
        .find(|e| type_of(e) == "RunError")
        .expect("RunError");
    assert_eq!(error["task_id"], "tk_1");
    assert_eq!(error["event"]["code"], "capacity");
    assert_eq!(error["event"]["reason"], "error");
}

// ---- an interrupt answer typed on the runner (M2B-010a, tunnel.md §3.6a) --------------------

/// A hosted thread paused on the approval `int_1`, as a runner reports it.
fn paused_on_interrupt(runner: &mut Runner, thread: &str) {
    runner.open_thread(thread);
    runner.event(user_message(thread));
    runner.event(run_event(thread, started()));
    runner.event(run_event(
        thread,
        json!({
            "type": "Interrupt",
            "interrupt_id": "int_1",
            "kind": "approval",
            "title": "Run cargo test?",
            "expires_at": format_timestamp(now_ms() + 10 * 60 * 1000),
            "options": [
                { "option_id": "allow", "name": "Allow", "kind": "allow_once" },
                { "option_id": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }),
    ));
    runner.event(run_event(
        thread,
        json!({"type": "RunFinished", "outcome": {"type": "interrupt", "interrupt_ids": ["int_1"]}}),
    ));
    runner.read_acks();
    assert!(runner.nacks.is_empty(), "nacks: {:?}", runner.nacks);
}

fn answer_up(runner: &mut Runner, answer_id: &str, interrupt: &str, option: &str) {
    runner.next_event += 1;
    let event_id = format!("evt_ans_{answer_id}_{}", runner.next_event);
    runner.up_frame(json!({
        "t": "interrupt_answer",
        "event_id": event_id,
        "thread_id": runner.thread.clone(),
        "interrupt_id": interrupt,
        "answer_id": answer_id,
        "outcome": {"type": "selected", "option_id": option},
    }));
}

fn phone_answer(thread: &str, key: &str, option: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_1"),
        true,
        Some(key),
        Some(&json!({"outcome": {"type": "selected", "option_id": option}})),
    )
}

fn resolutions(thread: &str) -> Vec<Value> {
    thread_events(thread)
        .into_iter()
        .filter(|e| type_of(e) == "InterruptResolved")
        .collect()
}

/// The terminal's answer reaches the Thread DO first: it wins, the runner gets the `approval`
/// command like a phone's answer would, and the phone's answer is a 409 naming the terminal.
#[test]
fn a_runner_answer_that_reaches_the_hub_first_wins_and_the_phone_gets_409() {
    let machine = unique("mch", "ansfirst");
    let thread = unique("th", "ansfirst");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    runner.thread = thread.clone();
    paused_on_interrupt(&mut runner, &thread);

    answer_up(&mut runner, "ans_first", "int_1", "allow");
    runner.read_results(1);
    let result = &runner.results[0];
    assert_eq!(result["answer_id"], "ans_first");
    assert_eq!(result["status"], "accepted", "{result}");
    let local = format!("local:{machine}");
    assert_eq!(result["resolved_by"]["id"], local.as_str());

    // The winning answer reaches the runner as an `approval` command, like a phone's.
    runner.read_commands(1);
    let command = &runner.commands[0]["command"];
    assert_eq!(command["kind"], "approval");
    assert_eq!(command["interrupt_id"], "int_1");
    assert_eq!(command["outcome"]["option_id"], "allow");
    assert_eq!(command["resolved_by"]["id"], local.as_str());

    // The phone is too late.
    let late = phone_answer(&thread, "idk_ansfirst_phone", "reject");
    assert_eq!(late.status, 409, "late phone answer: {}", late.body);
    assert_eq!(late.json()["error"], "already_resolved");
    assert_eq!(late.json()["resolved_by"]["id"], local.as_str());

    // The hub's log has the terminal's answer once.
    let resolved = resolutions(&thread);
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["event"]["outcome"]["option_id"], "allow");
    assert_eq!(resolved[0]["event"]["resolved_by"]["id"], local.as_str());

    // An answer to an interrupt that does not exist is refused with a typed result.
    answer_up(&mut runner, "ans_unknown", "int_9", "allow");
    runner.read_results(2);
    assert_eq!(runner.results[1]["status"], "rejected");
    assert!(runner.results[1]["resolved_by"].is_null());
    assert!(
        runner.results[1]["message"]
            .as_str()
            .is_some_and(|m| m.contains("int_9")),
        "{}",
        runner.results[1]
    );
}

/// The phone answers first: the runner's answer is refused with `already_resolved` naming the
/// phone's user, and the only `approval` command the runner gets is the phone's.
#[test]
fn a_runner_answer_that_loses_to_the_phone_is_refused_and_the_agent_runs_the_phone_choice() {
    let machine = unique("mch", "anslate");
    let thread = unique("th", "anslate");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    runner.thread = thread.clone();
    paused_on_interrupt(&mut runner, &thread);

    let won = phone_answer(&thread, "idk_anslate_phone", "allow");
    assert_eq!(won.status, 200, "phone answer: {}", won.body);

    answer_up(&mut runner, "ans_late", "int_1", "reject");
    runner.read_results(1);
    let result = &runner.results[0];
    assert_eq!(result["answer_id"], "ans_late");
    assert_eq!(result["status"], "already_resolved", "{result}");
    assert_eq!(result["resolved_by"]["id"], "usr_dev");
    assert_eq!(result["resolved_by"]["device_id"], "dev_dev");

    // Exactly one approval reaches the runner, and it is the phone's choice.
    runner.read_commands(1);
    runner.ack_down();
    let approvals: Vec<&Value> = runner
        .commands
        .iter()
        .filter(|c| c["command"]["kind"] == "approval")
        .collect();
    assert_eq!(approvals.len(), 1, "{:?}", runner.commands);
    assert_eq!(approvals[0]["command"]["outcome"]["option_id"], "allow");
    assert_eq!(approvals[0]["command"]["resolved_by"]["id"], "usr_dev");

    let resolved = resolutions(&thread);
    assert_eq!(resolved.len(), 1, "{resolved:?}");
    assert_eq!(resolved[0]["event"]["resolved_by"]["id"], "usr_dev");
    assert_eq!(resolved[0]["event"]["outcome"]["option_id"], "allow");
}

// ---- a prompt typed on the runner (M2B-019d, tunnel.md §3.6b) -------------------------------

fn prompt_up(runner: &mut Runner, task: &str, text: &str) {
    runner.next_event += 1;
    let event_id = format!("evt_prompt_{task}_{}", runner.next_event);
    runner.up_frame(json!({
        "t": "prompt_submit",
        "event_id": event_id,
        "thread_id": runner.thread.clone(),
        "task_id": task,
        "message_id": format!("msg_{task}"),
        "parts": [{"type": "text", "text": text}],
    }));
}

/// A task as the runner reports it: `RunStarted` then `RunFinished`.
fn run_task(runner: &mut Runner, thread: &str, task: &str) {
    for event in [started(), finished()] {
        runner.event(envelope(thread, Some(task), Some("run_x"), event));
    }
    runner.read_acks();
    assert!(runner.nacks.is_empty(), "nacks: {:?}", runner.nacks);
}

/// A line typed on the runner while a phone's task runs goes up, is queued by the Thread DO
/// between the phone's prompts, and comes back down as a `prompt` command in queue order, with
/// the machine's local user as the author. The runner never decides the order.
#[test]
fn a_runner_typed_prompt_is_queued_by_the_hub_with_the_phones_and_comes_back_in_queue_order() {
    let machine = unique("mch", "localprompt");
    let thread = unique("th", "localprompt");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    runner.thread = thread.clone();
    runner.open_thread(&thread);
    runner.read_acks();

    // The phone starts a task; the runner gets its prompt.
    let started_reply = prompt(&thread, "tk_ph1", "idk_localprompt_1");
    assert_eq!(started_reply.status, 202, "{}", started_reply.body);
    runner.read_commands(1);
    runner.ack_down();

    // The line typed on the runner goes up while that task runs; then the phone queues another.
    prompt_up(&mut runner, "tk_loc", "typed on the runner");
    runner.read_acks();
    assert!(runner.nacks.is_empty(), "nacks: {:?}", runner.nacks);
    let queued = prompt(&thread, "tk_ph2", "idk_localprompt_2");
    assert_eq!(queued.status, 202, "{}", queued.body);
    assert_eq!(queued.json()["status"], "pending");
    assert_eq!(queued.json()["position"], 2, "behind the runner's line");
    assert_eq!(
        runner.commands.len(),
        1,
        "nothing else is sent while a task runs"
    );

    // The first task ends: the hub sends the runner's own line, authored by the local user.
    run_task(&mut runner, &thread, "tk_ph1");
    runner.read_commands(2);
    runner.ack_down();
    let local = &runner.commands[1]["command"];
    assert_eq!(local["kind"], "prompt");
    assert_eq!(local["task_id"], "tk_loc");
    assert_eq!(local["message_id"], "msg_tk_loc");
    assert_eq!(local["parts"][0]["text"], "typed on the runner");
    let local_user = format!("local:{machine}");
    assert_eq!(local["author"]["id"], local_user.as_str());
    assert_eq!(
        runner.commands.len(),
        2,
        "the phone's second prompt waits its turn"
    );

    // It ends: now the phone's second prompt.
    run_task(&mut runner, &thread, "tk_loc");
    runner.read_commands(3);
    assert_eq!(runner.commands[2]["command"]["task_id"], "tk_ph2");

    // The hub's log has each prompt once, in the hub's order.
    let messages: Vec<(String, String)> = thread_events(&thread)
        .iter()
        .filter(|e| type_of(e) == "UserMessage")
        .map(|e| {
            (
                e["task_id"].as_str().unwrap_or_default().to_owned(),
                e["event"]["author"]["id"]
                    .as_str()
                    .unwrap_or_default()
                    .to_owned(),
            )
        })
        .collect();
    assert_eq!(
        messages,
        vec![
            ("tk_ph1".to_owned(), "usr_dev".to_owned()),
            ("tk_loc".to_owned(), local_user),
            ("tk_ph2".to_owned(), "usr_dev".to_owned()),
        ]
    );
}

/// A prompt for a thread the machine does not host is nacked, and a retransmit of an accepted
/// one on a fresh stream (same `event_id`) queues one task.
#[test]
fn a_runner_prompt_for_a_foreign_thread_is_nacked_and_a_retransmit_queues_one_task() {
    let machine = unique("mch", "localnack");
    let thread = unique("th", "localnack");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    runner.thread = unique("th", "foreign");
    prompt_up(&mut runner, "tk_x", "nobody hosts me");
    runner.read_acks();
    assert_eq!(runner.nacks.len(), 1, "{:?}", runner.nacks);
    assert_eq!(runner.nacks[0]["code"], "forbidden");

    runner.thread = thread.clone();
    runner.open_thread(&thread);
    prompt_up(&mut runner, "tk_once", "only once");
    // The hub processes both frames (and sends the prompt down), but the runner never reads
    // the acks.
    wait_state(&machine, "three frames processed", |s| s["up_last"] == 3);
    let saved = runner.crash(None);
    wait_state(&machine, "the loss to be noticed", |s| {
        s["connected"] == false
    });

    // Restarted from an old backup: no token, the outbox still holds both frames.
    let restarted = Persisted {
        token: None,
        last_down: 0,
        ..saved
    };
    let mut runner = Runner::connect(&machine, &credential, restarted);
    assert!(!runner.resumed(), "welcome: {}", runner.welcome);
    runner.read_acks();
    let tasks: Vec<String> = thread_events(&thread)
        .iter()
        .filter(|e| type_of(e) == "UserMessage")
        .map(|e| e["task_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(tasks, ["tk_once"]);
}

// ---- a task the runner started before /rc (M2B-019f, tunnel.md §3.4) -------------------------

/// `/rc` while the agent runs a task the runner started offline: `thread_open` names it, so the
/// hub counts it busy from the moment the thread exists. A phone prompt that arrives before the
/// runner's history does waits in the queue (it is not sent down to a bridge that would refuse
/// it), and goes down once, after the offline task ended.
#[test]
fn a_task_started_before_the_thread_was_opened_makes_the_first_phone_prompt_wait() {
    let machine = unique("mch", "offlinetask");
    let thread = unique("th", "offlinetask");
    let credential = dev_credential(&machine);
    let mut runner = Runner::first(&machine, &credential);
    runner.thread = thread.clone();
    runner.up_frame(json!({
        "t": "thread_open",
        "event_id": format!("evt_open_{thread}"),
        "thread_id": thread,
        "title": "typed offline",
        "agent": "claude",
        "workspace": {"path": "/tmp/work"},
        "running_task": "tk_off",
    }));
    runner.read_acks();
    assert!(runner.nacks.is_empty(), "nacks: {:?}", runner.nacks);

    // The phone prompts before the runner's events reach the hub: it waits.
    let reply = prompt(&thread, "tk_ph1", "idk_offlinetask_1");
    assert_eq!(reply.status, 202, "{}", reply.body);
    assert_eq!(reply.json()["status"], "pending", "{}", reply.body);
    assert_eq!(reply.json()["position"], 1);

    // The runner's history follows: the offline task's own message, run and end.
    runner.event(envelope(
        &thread,
        Some("tk_off"),
        None,
        json!({
            "type": "UserMessage",
            "message_id": "msg_off",
            "parts": [{"type": "text", "text": "typed offline"}],
            "author": {"kind": "user", "id": "local", "name": "terminal"},
        }),
    ));
    runner.event(envelope(
        &thread,
        Some("tk_off"),
        Some("run_off"),
        started(),
    ));
    runner.read_acks();
    assert!(runner.nacks.is_empty(), "nacks: {:?}", runner.nacks);
    assert!(
        runner.commands.is_empty(),
        "nothing goes down while the offline task runs: {:?}",
        runner.commands
    );

    // It ends: the phone's prompt goes down, once.
    runner.event(envelope(
        &thread,
        Some("tk_off"),
        Some("run_off"),
        finished(),
    ));
    runner.read_commands(1);
    runner.ack_down();
    assert_eq!(runner.commands.len(), 1);
    assert_eq!(runner.commands[0]["command"]["task_id"], "tk_ph1");
    let messages: Vec<String> = thread_events(&thread)
        .iter()
        .filter(|e| type_of(e) == "UserMessage")
        .map(|e| e["task_id"].as_str().unwrap_or_default().to_owned())
        .collect();
    assert_eq!(messages, ["tk_off", "tk_ph1"]);
}
