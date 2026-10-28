//! M2B-014 live tests: a machine that comes back online while commands are queued for it sends
//! one "machine back" push to each of the org's subscriptions, and a comeback inside the debounce
//! window sends none.
//!
//! The runner side is the hand-written tunnel client of `machine_do.rs` and the push side the fake
//! push service of `notify.rs`, both copied here. Needs `HUB_URL` and the per-run VAPID key that
//! `bash scripts/hub-test.sh presence_notify` sets up. The `live` feature (required by
//! `[[test]] presence_notify` in Cargo.toml) keeps this file out of a plain `cargo test -p hub`.
//!
//! Every notification of the dev org reaches the dev user's subscriptions, so the tests run one at
//! a time (`serial()`), reset the prefs and remove their subscriptions at the end.

use std::io::{Read, Write};
use std::net::{Shutdown, TcpListener, TcpStream};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub::notify::webpush::{b64_encode, decrypt};
use p256::SecretKey;
use p256::elliptic_curve::sec1::ToEncodedPoint;
use protocol::tunnel::{Receiver, Sender, Verdict};
use serde_json::{Value, json};

/// How long to wait for a push that must arrive (the queue batches for about a second).
const PUSH_WAIT: Duration = Duration::from_secs(25);
/// How long to wait to be sure a push does not arrive.
const QUIET_WAIT: Duration = Duration::from_secs(4);

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
    /// Command frames received, in order, duplicates dropped.
    commands: Vec<Value>,
    nacks: Vec<Value>,
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
            commands: Vec::new(),
            nacks: Vec::new(),
            next_event: saved.next_event,
        };
        runner.flush();
        runner
    }

    /// Puts every frame the window allows on the wire.
    fn flush(&mut self) {
        while let Some((seq, mut body)) = self.up.next_to_send() {
            body["seq"] = json!(seq);
            self.ws.send_text(&body.to_string());
        }
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

    /// Reads until `n` commands were received in total.
    fn read_commands(&mut self, n: usize) {
        self.read_until(&format!("{n} commands"), |r| r.commands.len() >= n);
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
fn unique(prefix: &str, name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{prefix}_{name}_{}_{nanos:x}", std::process::id())
}

/// A named machine through the admin API (its row gives the push title its name): `(id, credential)`.
fn named_machine(name: &str) -> (String, String) {
    let reply = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({"name": name, "kind": "local"})),
    );
    assert_eq!(reply.status, 201, "create machine: {}", reply.body);
    let body = reply.json();
    (
        body["machine"]["id"].as_str().expect("id").to_owned(),
        body["credential"].as_str().expect("credential").to_owned(),
    )
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

fn dechunk_bytes(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = find(rest, b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..line_end]).into_owned();
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        let after = &rest[line_end + 2..];
        if size == 0 || after.len() < size {
            break;
        }
        out.extend_from_slice(&after[..size]);
        rest = after.get(size + 2..).unwrap_or_default();
    }
    out
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack
        .windows(needle.len())
        .position(|window| window == needle)
}

// ---- the fake push service --------------------------------------------------------------

/// One request the fake push service received; only the body matters to these tests.
#[derive(Clone)]
struct Received {
    body: Vec<u8>,
}

/// A push service on a loopback port: records every POST and answers 201.
struct PushService {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
}

impl PushService {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake push service");
        let port = listener.local_addr().expect("addr").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&received);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let log = Arc::clone(&log);
                std::thread::spawn(move || serve(stream, &log));
            }
        });
        Self { port, received }
    }

    fn endpoint(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/push/{name}", self.port)
    }

    fn count(&self) -> usize {
        self.received.lock().unwrap().len()
    }

    /// The `n`th request (0-based), waiting up to [`PUSH_WAIT`] for it.
    fn wait_for(&self, n: usize) -> Received {
        let deadline = Instant::now() + PUSH_WAIT;
        loop {
            if let Some(request) = self.received.lock().unwrap().get(n) {
                return request.clone();
            }
            assert!(
                Instant::now() < deadline,
                "no push request #{n} reached the fake push service in {PUSH_WAIT:?}"
            );
            std::thread::sleep(Duration::from_millis(100));
        }
    }

    /// Asserts that no more than `expected` requests arrive within [`QUIET_WAIT`].
    fn assert_no_more_than(&self, expected: usize) {
        std::thread::sleep(QUIET_WAIT);
        assert_eq!(self.count(), expected, "unexpected push requests");
    }
}

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Received>>) {
    stream.set_read_timeout(Some(READ_TIMEOUT)).ok();
    let mut buffer = Vec::new();
    let mut chunk = [0_u8; 4096];
    let head_end = loop {
        if let Some(end) = find(&buffer, b"\r\n\r\n") {
            break end;
        }
        match stream.read(&mut chunk) {
            Ok(0) | Err(_) => return,
            Ok(n) => buffer.extend_from_slice(&chunk[..n]),
        }
    };
    let head = String::from_utf8_lossy(&buffer[..head_end]).into_owned();
    let headers: Vec<(String, String)> = head
        .split("\r\n")
        .skip(1)
        .filter_map(|line| line.split_once(':'))
        .map(|(n, v)| (n.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let mut body = buffer[head_end + 4..].to_vec();
    let length = headers
        .iter()
        .find(|(n, _)| n == "content-length")
        .and_then(|(_, v)| v.parse::<usize>().ok());
    let chunked = headers
        .iter()
        .any(|(n, v)| n == "transfer-encoding" && v.contains("chunked"));
    if let Some(length) = length {
        while body.len() < length {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
    } else if chunked {
        while !body.ends_with(b"0\r\n\r\n") {
            match stream.read(&mut chunk) {
                Ok(0) | Err(_) => break,
                Ok(n) => body.extend_from_slice(&chunk[..n]),
            }
        }
        body = dechunk_bytes(&body);
    }
    log.lock().unwrap().push(Received { body });
    let reply = "HTTP/1.1 201 X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
    stream.write_all(reply.as_bytes()).ok();
}

// ---- helpers ----------------------------------------------------------------------------

fn serial() -> MutexGuard<'static, ()> {
    static LOCK: Mutex<()> = Mutex::new(());
    LOCK.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// A browser's push subscription keys; the private half stays in the test.
struct Browser {
    secret: SecretKey,
    auth: [u8; 16],
}

impl Browser {
    fn new() -> Self {
        let mut auth = [0_u8; 16];
        getrandom::getrandom(&mut auth).expect("random");
        loop {
            let mut scalar = [0_u8; 32];
            getrandom::getrandom(&mut scalar).expect("random");
            if let Ok(secret) = SecretKey::from_slice(&scalar) {
                return Self { secret, auth };
            }
        }
    }

    fn subscription(&self, endpoint: &str) -> Value {
        let public = self.secret.public_key().to_encoded_point(false);
        json!({
            "endpoint": endpoint,
            "keys": { "p256dh": b64_encode(public.as_bytes()), "auth": b64_encode(&self.auth) },
        })
    }

    fn open(&self, request: &Received) -> Value {
        let plain = decrypt(&request.body, &self.secret, &self.auth)
            .expect("the push body decrypts with the subscription's private key");
        serde_json::from_slice(&plain).expect("payload is JSON")
    }
}

// ---- the subscriptions ------------------------------------------------------------------

fn reset_prefs() {
    let reply = http("PUT", "/v1/push/prefs", true, None, Some(&json!({})));
    assert_eq!(reply.status, 200, "reset prefs: {}", reply.body);
}

fn subscribe(browser: &Browser, endpoint: &str) -> Reply {
    http(
        "POST",
        "/v1/push/subscriptions",
        true,
        None,
        Some(&browser.subscription(endpoint)),
    )
}

fn unsubscribe(endpoint: &str) {
    let reply = http(
        "DELETE",
        "/v1/push/subscriptions",
        true,
        None,
        Some(&json!({ "endpoint": endpoint })),
    );
    assert_eq!(reply.status, 204, "unsubscribe: {}", reply.body);
}

/// A fresh push service, a browser subscribed to it under `name`, default prefs.
fn setup(name: &str) -> (PushService, Browser, String) {
    reset_prefs();
    let service = PushService::start();
    let browser = Browser::new();
    let endpoint = service.endpoint(name);
    let reply = subscribe(&browser, &endpoint);
    assert_eq!(reply.status, 201, "subscribe: {}", reply.body);
    (service, browser, endpoint)
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn a_machine_back_with_queued_work_sends_one_push_with_ids_and_a_generic_title() {
    let _serial = serial();
    let (service, browser, endpoint) = setup("back");
    // A named machine: its name must not reach the push (the phone looks it up itself).
    let (machine, credential) = named_machine("Garage Mac");
    let thread = unique("th", "back");
    init_thread(&thread, &machine);

    // The machine is offline, so the prompt waits in its outbox.
    let reply = prompt(&thread, "tk_1", &unique("idk", "back"));
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);
    assert_eq!(machine_state(&machine)["outbox_len"], 1);

    // The runner comes back with the prompt still unacked: one push.
    let mut runner = Runner::first(&machine, &credential);
    runner.read_commands(1);
    let payload = browser.open(&service.wait_for(0));
    assert_eq!(payload["kind"], "machine_back");
    assert_eq!(payload["title"], "Machine back online");
    assert_eq!(payload["thread_id"], thread.as_str());
    assert_eq!(payload["machine_id"], machine.as_str());
    let text = payload.to_string();
    assert!(!text.contains("do it"), "prompt text in a push: {text}");
    assert!(
        !text.contains("Garage Mac"),
        "machine name in a push: {text}"
    );
    service.assert_no_more_than(1);

    unsubscribe(&endpoint);
    drop(runner);
}

#[test]
fn a_quick_reconnect_within_the_window_sends_no_second_push() {
    let _serial = serial();
    let (service, _browser, endpoint) = setup("flap");
    let machine = unique("mch", "flap");
    let thread = unique("th", "flap");
    let credential = dev_credential(&machine);
    init_thread(&thread, &machine);
    let reply = prompt(&thread, "tk_1", &unique("idk", "flap"));
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);

    let mut runner = Runner::first(&machine, &credential);
    runner.read_commands(1);
    service.wait_for(0);

    // The socket drops and the runner is back at once, still with the prompt unacked. The
    // comeback is inside the debounce window, so it sends nothing.
    let saved = runner.crash(None);
    wait_state(&machine, "offline", |s| s["presence"] == "offline");
    let _again = Runner::connect(&machine, &credential, saved);
    wait_state(&machine, "online", |s| s["presence"] == "online");
    service.assert_no_more_than(1);

    unsubscribe(&endpoint);
}

#[test]
fn a_machine_back_with_nothing_queued_sends_no_push() {
    let _serial = serial();
    let (service, _browser, endpoint) = setup("idle");
    let machine = unique("mch", "idle");
    let thread = unique("th", "idle");
    let credential = dev_credential(&machine);
    init_thread(&thread, &machine);

    let _runner = Runner::first(&machine, &credential);
    wait_state(&machine, "online", |s| s["presence"] == "online");
    service.assert_no_more_than(0);

    unsubscribe(&endpoint);
}

#[test]
fn a_machine_back_goes_to_every_subscription_of_the_user() {
    let _serial = serial();
    let (service, browser, endpoint) = setup("device-a");
    // The same user's second device (a second endpoint of the same browser keys).
    let second = service.endpoint("device-b");
    let reply = subscribe(&browser, &second);
    assert_eq!(reply.status, 201, "second subscribe: {}", reply.body);
    let machine = unique("mch", "devices");
    let thread = unique("th", "devices");
    let credential = dev_credential(&machine);
    init_thread(&thread, &machine);
    let reply = prompt(&thread, "tk_1", &unique("idk", "devices"));
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);

    let mut runner = Runner::first(&machine, &credential);
    runner.read_commands(1);
    // One push on each of the two subscriptions, both with the machine id.
    for n in 0..2 {
        let payload = browser.open(&service.wait_for(n));
        assert_eq!(payload["kind"], "machine_back");
        assert_eq!(payload["machine_id"], machine.as_str());
        assert_eq!(payload["title"], "Machine back online");
    }
    service.assert_no_more_than(2);

    unsubscribe(&endpoint);
    unsubscribe(&second);
    drop(runner);
}
