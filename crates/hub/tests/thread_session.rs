//! M2-008b live tests of `POST /v1/threads/{id}/session` (client-api.md §5.5a) against a running
//! hub (`wrangler dev --local`).
//!
//! Needs `HUB_URL` and a hub started with `DEV_FIXTURES=1` (`crates/hub/.dev.vars` does that for
//! `wrangler dev`). Run it through `bash scripts/hub-test.sh thread_session`, which builds the
//! Worker, starts wrangler and sets `HUB_URL`. Without `HUB_URL` every test fails with a message
//! saying so. The file is compiled only with the `live` feature (`scripts/hub-test.sh` turns it
//! on), so a plain `cargo test -p hub` skips it.
//!
//! The fake runner is a hand-written tunnel client on `std::net` (the same contract as
//! `tests/thread_create.rs`): `hello`, `welcome`, then the `command` frames the Machine DO queued
//! for the thread. The thread's `session` state is seeded through the dev event route, as a
//! runner's `session/new` snapshot would put it there.

#![cfg(feature = "live")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::tunnel::{Receiver, Verdict};
use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const READ_TIMEOUT: Duration = Duration::from_secs(20);
const WAIT: Duration = Duration::from_secs(15);

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
             Run it with `bash scripts/hub-test.sh thread_session`."
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

/// One request with `Connection: close`, as `Bearer dev` unless `auth` is false.
fn http(
    method: &str,
    path: &str,
    auth: bool,
    idempotency_key: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    http_as(method, path, auth.then_some("dev"), idempotency_key, body)
}

/// As [`http`], with `Authorization: Bearer <token>` when `token` is given.
fn http_as(
    method: &str,
    path: &str,
    token: Option<&str>,
    idempotency_key: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let mut stream = connect();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        authority()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
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
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
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

struct Ws {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    /// Opens the tunnel with the machine's credential (subprotocol `sm.tunnel.v1`).
    fn tunnel(machine: &str, credential: &str) -> Self {
        let mut stream = connect();
        let request = format!(
            "GET /v1/machines/{machine}/tunnel HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: sm.tunnel.v1\r\n\
             Authorization: Bearer {credential}\r\n\r\n",
            authority()
        );
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
        assert!(head.starts_with("HTTP/1.1 101"), "tunnel refused: {head}");
        Self {
            stream,
            buf: Vec::new(),
        }
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
        self.stream.write_all(&frame).expect("send frame");
    }

    /// The next text frame as JSON, or `None` when nothing arrives within `wait`.
    fn recv_within(&mut self, wait: Duration) -> Option<Value> {
        self.stream
            .set_read_timeout(Some(wait))
            .expect("set read timeout");
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        let text = String::from_utf8_lossy(&payload).into_owned();
                        return Some(serde_json::from_str(&text).unwrap_or_else(|error| {
                            panic!("frame is not JSON ({error}): {text}")
                        }));
                    }
                    0x8 => panic!("the hub closed the tunnel"),
                    _ => continue,
                }
            }
            let mut chunk = [0u8; 8192];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("the tunnel connection closed"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(_) => return None,
            }
        }
    }

    /// The next frame, which must come within the test's wait.
    fn next(&mut self) -> Value {
        self.recv_within(WAIT)
            .unwrap_or_else(|| panic!("no frame from the hub within {WAIT:?}"))
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

// ---- the fake runner --------------------------------------------------------------------

/// A runner that has connected: its `welcome` and the `command` frames it has accepted.
struct Runner {
    ws: Ws,
    down: Receiver,
    commands: Vec<Value>,
}

impl Runner {
    /// `hello` (no resume token: a first connection), `welcome`, then the command frames.
    fn connect(machine: &str, credential: &str) -> Self {
        let mut ws = Ws::tunnel(machine, credential);
        ws.send_text(
            &json!({
                "t": "hello",
                "proto": 1,
                "machine_id": machine,
                "runner_version": "0.0.0-test",
                "resume_token": null,
                "last_down_seq": 0,
                "capabilities": {
                    "host_mode": "local",
                    "harnesses": ["claude"],
                    "max_sessions": 4,
                    "spawn": ["same_dir", "worktree"],
                    "features": [],
                },
            })
            .to_string(),
        );
        // The first frame after hello is welcome; anything after it is a command.
        let welcome = ws.next();
        assert_eq!(welcome["t"], "welcome", "first frame: {welcome}");
        Self {
            ws,
            down: Receiver::fresh(),
            commands: Vec::new(),
        }
    }

    /// Reads command frames until `n` arrived (acking each one, as the runner must).
    fn read_commands(&mut self, n: usize) {
        while self.commands.len() < n {
            let frame = self.ws.next();
            assert_eq!(
                frame["t"], "command",
                "unexpected frame after welcome: {frame}"
            );
            let seq = frame["seq"].as_u64().expect("seq");
            if let Ok(Verdict::Process) = self.down.on_frame(seq) {
                self.commands.push(frame);
            }
            self.ws
                .send_text(&json!({ "t": "ack", "seq": self.down.last() }).to_string());
        }
    }
}

// ---- helpers ----------------------------------------------------------------------------

fn unique(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("{name}_{}_{nanos:x}", std::process::id())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

/// A machine of the dev org, created through `POST /v1/machines` (so it has a D1 row, which
/// `POST /v1/threads` requires): `(machine id, credential)`.
fn dev_machine() -> (String, String) {
    let reply = http(
        "POST",
        "/v1/machines",
        true,
        None,
        Some(&json!({ "name": unique("test-machine") })),
    );
    assert_eq!(reply.status, 201, "create machine: {}", reply.body);
    let out = reply.json();
    (
        out["machine"]["id"]
            .as_str()
            .expect("machine id")
            .to_owned(),
        out["credential"].as_str().expect("credential").to_owned(),
    )
}

/// The Machine DO's queued commands, through the dev state route.
fn outbox_len(machine: &str) -> u64 {
    let reply = http(
        "GET",
        &format!("/v1/dev/machines/{machine}/state"),
        true,
        None,
        None,
    );
    assert_eq!(reply.status, 200, "machine state: {}", reply.body);
    reply.json()["outbox_len"].as_u64().expect("outbox_len")
}

/// The thread id the hub derives for the dev user and `key` (`threads/routes.rs`).
fn derived_thread_id(key: &str) -> String {
    let digest = Sha256::digest(format!("usr_dev\n{key}").as_bytes());
    let hex: String = digest.iter().take(16).map(|b| format!("{b:02x}")).collect();
    format!("th_{hex}")
}

/// An idle thread (no prompt) on `machine`: its id.
fn idle_thread(machine: &str) -> String {
    let body = json!({
        "machine_id": machine,
        "agent": "claude",
        "workspace": { "path": "/tmp/work", "spawn": "worktree" },
        "title": "Tune the session",
    });
    let reply = http(
        "POST",
        "/v1/threads",
        true,
        Some(&unique("idk_thread")),
        Some(&body),
    );
    assert_eq!(reply.status, 201, "create thread: {}", reply.body);
    assert_eq!(reply.json()["status"], "idle");
    reply.json()["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_owned()
}

/// Puts a `session` state into the thread, as a runner's `session/new` snapshot would: two modes
/// and two config options (a select and a boolean).
fn seed_session_state(thread: &str) {
    let snapshot = json!({
        "available_commands": [],
        "current_mode_id": "code",
        "modes": [
            { "id": "code", "name": "Code" },
            { "id": "plan", "name": "Plan" }
        ],
        "config_options": [
            {
                "id": "model", "name": "Model", "type": "select", "currentValue": "sonnet",
                "options": [
                    { "value": "sonnet", "name": "Sonnet" },
                    { "value": "opus", "name": "Opus" }
                ]
            },
            { "id": "auto_fix", "name": "Auto fix", "type": "boolean", "currentValue": false }
        ]
    });
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        true,
        None,
        Some(&json!({
            "v": 1,
            "seq": 0,
            "thread_id": thread,
            "task_id": null,
            "run_id": null,
            "ts": format_timestamp(now_ms()),
            "event": { "type": "StateSnapshot", "scope": "session", "snapshot": snapshot },
        })),
    );
    assert_eq!(reply.status, 200, "session snapshot: {}", reply.body);
}

fn session(thread: &str, key: &str, body: &Value) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/session"),
        true,
        Some(key),
        Some(body),
    )
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn a_session_change_is_queued_and_the_runner_gets_it_after_welcome() {
    let (machine, credential) = dev_machine();
    let thread = idle_thread(&machine);
    seed_session_state(&thread);

    let key = unique("idk_session");
    let body = json!({ "mode_id": "plan", "config": { "id": "model", "value": "opus" } });
    let answer = session(&thread, &key, &body);
    assert_eq!(answer.status, 202, "session: {}", answer.body);
    assert_eq!(answer.json()["status"], "queued");
    assert_eq!(outbox_len(&machine), 1, "one session_config queued");

    // A replay under the same key gives the same answer and queues nothing more.
    let replay = session(&thread, &key, &body);
    assert_eq!(replay.status, 202, "replay: {}", replay.body);
    assert_eq!(
        replay.json(),
        answer.json(),
        "the stored answer is replayed"
    );
    assert_eq!(outbox_len(&machine), 1, "the replay queued nothing");

    // The runner connects after the write; the command arrives after welcome (tunnel.md §5).
    let mut runner = Runner::connect(&machine, &credential);
    runner.read_commands(1);
    let command = &runner.commands[0];
    assert_eq!(command["command"]["kind"], "session_config", "{command}");
    assert_eq!(command["idempotency_key"], key.as_str());
    let change = &command["command"];
    assert_eq!(change["thread_id"], thread.as_str());
    assert_eq!(change["mode_id"], "plan");
    assert_eq!(change["config"], json!({ "id": "model", "value": "opus" }));
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "exactly one command for one change"
    );

    // The change is audited (`thread.session`), with the thread it names.
    let audit = http("GET", "/v1/dev/audit?limit=1000", true, None, None);
    assert_eq!(audit.status, 200, "audit: {}", audit.body);
    assert!(
        audit.body.contains("thread.session") && audit.body.contains(&thread),
        "no thread.session row for {thread}"
    );
}

#[test]
fn a_refused_session_change_is_400_and_queues_nothing() {
    let (machine, credential) = dev_machine();
    let thread = idle_thread(&machine);
    seed_session_state(&thread);

    let refused = [
        json!({}),
        json!({ "mode_id": "nope" }),
        json!({ "config": { "id": "model", "value": "gpt" } }),
        json!({ "config": { "id": "auto_fix", "value": "yes" } }),
    ];
    for body in refused {
        let reply = session(&thread, &unique("idk_refused"), &body);
        assert_eq!(reply.status, 400, "{body} was accepted: {}", reply.body);
        assert_eq!(reply.json()["error"], "invalid_request", "{}", reply.body);
    }
    assert_eq!(outbox_len(&machine), 0, "a refused change was queued");
    let mut runner = Runner::connect(&machine, &credential);
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "no command reached the runner"
    );
}

#[test]
fn a_thread_that_does_not_exist_is_404_and_nothing_is_written() {
    let (machine, _) = dev_machine();
    let key = unique("idk_missing");
    let thread = derived_thread_id(&key);
    let reply = session(&thread, &key, &json!({ "mode_id": "plan" }));
    assert_eq!(reply.status, 404, "{}", reply.body);
    assert_eq!(reply.json()["error"], "not_found", "{}", reply.body);
    assert_eq!(outbox_len(&machine), 0);
}

#[test]
fn a_viewer_cannot_change_the_session() {
    let issued = http(
        "POST",
        "/v1/auth/sessions",
        true,
        None,
        Some(&json!({ "role": "viewer", "display_name": unique("viewer") })),
    );
    assert_eq!(issued.status, 201, "issue viewer: {}", issued.body);
    let viewer = issued.json()["session"]["token"]
        .as_str()
        .expect("token")
        .to_owned();
    let (machine, _) = dev_machine();
    let thread = idle_thread(&machine);
    seed_session_state(&thread);

    let reply = http_as(
        "POST",
        &format!("/v1/threads/{thread}/session"),
        Some(&viewer),
        Some(&unique("idk_viewer")),
        Some(&json!({ "mode_id": "plan" })),
    );
    assert_eq!(reply.status, 403, "viewer session: {}", reply.body);
    assert_eq!(reply.json()["error"], "forbidden");
    assert_eq!(outbox_len(&machine), 0, "nothing was queued");
}
