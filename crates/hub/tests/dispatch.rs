//! M2B-013 live tests of dispatching a task to a chosen machine (`POST /v1/threads`, plan 4.10)
//! against a running hub (`wrangler dev --local`), plus the machine admin routes the desktop's
//! machines screen uses (`PATCH` rename, `DELETE` revoke, `GET` list).
//!
//! Needs `HUB_URL` and a hub started with `DEV_FIXTURES=1`. Run it through
//! `bash scripts/hub-test.sh dispatch`. The file is compiled only with the `live` feature.
//!
//! The fake runner is a hand-written tunnel client on `std::net` (the contract of
//! `tests/machine_do.rs`): `hello` with chosen capabilities, `welcome`, then the `command` frames
//! the Machine DO delivers. Every test uses its own machine.

#![cfg(feature = "live")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use protocol::tunnel::{Receiver, Verdict};
use serde_json::{Value, json};

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
             Run it with `bash scripts/hub-test.sh dispatch`."
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
    /// A subscription to a thread (turn-event.md 6): returns the events up to `live`.
    fn thread_events(thread: &str) -> Vec<Value> {
        let mut stream = connect();
        let request = format!(
            "GET /v1/threads/{thread}/subscribe HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nAuthorization: Bearer dev\r\n\r\n",
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
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "subscribe refused: {head}"
        );
        let mut ws = Self {
            stream,
            buf: Vec::new(),
        };
        let mut events = Vec::new();
        loop {
            let message = ws.next();
            match message["type"].as_str() {
                Some("event") => events.push(message["envelope"].clone()),
                Some("live") => return events,
                _ => {}
            }
        }
    }

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
    welcome: Value,
    commands: Vec<Value>,
}

impl Runner {
    /// `hello` (no resume token: a first connection), `welcome`, then the command frames.
    fn connect(machine: &str, credential: &str) -> Self {
        Self::connect_with(machine, credential, &["same_dir", "worktree"], 4)
    }

    /// As [`Runner::connect`], advertising `spawn` modes and `max_sessions`.
    fn connect_with(machine: &str, credential: &str, spawn: &[&str], max_sessions: u32) -> Self {
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
                    "max_sessions": max_sessions,
                    "spawn": spawn,
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
            welcome,
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

/// A machine of the dev org created through `POST /v1/machines` (so it has a D1 row, which
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

/// The machine's entry of `GET /v1/machines`, if it is listed.
fn listed(machine: &str) -> Option<Value> {
    let reply = http("GET", "/v1/machines", true, None, None);
    assert_eq!(reply.status, 200, "list machines: {}", reply.body);
    reply.json()["machines"]
        .as_array()
        .and_then(|machines| machines.iter().find(|m| m["machine_id"] == machine))
        .cloned()
}

/// Polls the list until `ok` holds for the machine's entry.
fn wait_listed(machine: &str, what: &str, ok: impl Fn(&Value) -> bool) -> Value {
    let deadline = Instant::now() + WAIT;
    loop {
        if let Some(entry) = listed(machine)
            && ok(&entry)
        {
            return entry;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for {what}: {:?}",
            listed(machine)
        );
        std::thread::sleep(Duration::from_millis(100));
    }
}

fn create_body(machine: &str, spawn: &str) -> Value {
    json!({
        "machine_id": machine,
        "agent": "claude",
        "workspace": { "path": "/tmp/work", "spawn": spawn },
        "permission_mode": "default",
        "title": "Upgrade axum",
        "prompt": {
            "task_id": format!("tk_{}", unique("first")),
            "message_id": format!("msg_{}", unique("first")),
            "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }],
        },
    })
}

fn create(key: &str, body: &Value) -> Reply {
    http("POST", "/v1/threads", true, Some(key), Some(body))
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn a_task_reaches_the_chosen_machine_with_its_spawn_mode_and_no_other() {
    let (chosen, chosen_credential) = dev_machine();
    let (other, other_credential) = dev_machine();
    let mut runner = Runner::connect(&chosen, &chosen_credential);
    let mut decoy = Runner::connect(&other, &other_credential);

    let worktree_key = unique("idk_wt");
    let created = create(&worktree_key, &create_body(&chosen, "worktree"));
    assert_eq!(created.status, 201, "create: {}", created.body);
    assert_eq!(created.json()["status"], "started");
    let same_dir_key = unique("idk_sd");
    let created = create(&same_dir_key, &create_body(&chosen, "same_dir"));
    assert_eq!(created.status, 201, "create: {}", created.body);

    runner.read_commands(2);
    let spawn_of = |key: &str| {
        let command = runner
            .commands
            .iter()
            .find(|frame| frame["idempotency_key"] == key)
            .unwrap_or_else(|| panic!("no command under {key}: {:?}", runner.commands));
        assert_eq!(command["command"]["kind"], "dispatch");
        command["command"]["workspace"]["spawn"].clone()
    };
    assert_eq!(spawn_of(&worktree_key), "worktree");
    assert_eq!(spawn_of(&same_dir_key), "same_dir");
    assert!(
        decoy.ws.recv_within(Duration::from_millis(500)).is_none(),
        "another machine got nothing"
    );
    assert_eq!(outbox_len(&other), 0);
}

#[test]
fn a_spawn_mode_the_hub_or_the_machine_does_not_know_is_refused_and_nothing_is_queued() {
    let (machine, credential) = dev_machine();
    let _runner = Runner::connect_with(&machine, &credential, &["same_dir"], 4);
    wait_listed(&machine, "the hello capabilities", |entry| {
        entry["capabilities"]["spawn"] == json!(["same_dir"])
    });

    // A mode that does not exist, and one this machine did not advertise.
    for (mode, why) in [("clone", "unknown mode"), ("worktree", "not advertised")] {
        let refused = create(&unique("idk_bad"), &create_body(&machine, mode));
        assert_eq!(refused.status, 400, "{why}: {}", refused.body);
        assert_eq!(refused.json()["error"], "invalid_request", "{why}");
        assert!(
            refused.json()["message"]
                .as_str()
                .is_some_and(|m| !m.is_empty()),
            "{why}: the refusal says why"
        );
    }
    assert_eq!(outbox_len(&machine), 0, "nothing was queued");

    let accepted = create(&unique("idk_ok"), &create_body(&machine, "same_dir"));
    assert_eq!(accepted.status, 201, "same_dir: {}", accepted.body);
}

#[test]
fn a_machine_at_capacity_refuses_and_the_task_fails_with_the_capacity_code() {
    let (machine, credential) = dev_machine();
    let mut runner = Runner::connect_with(&machine, &credential, &["same_dir", "worktree"], 1);

    let key = unique("idk_cap");
    let body = create_body(&machine, "same_dir");
    let task = body["prompt"]["task_id"].as_str().expect("task").to_owned();
    let created = create(&key, &body);
    assert_eq!(created.status, 201, "create: {}", created.body);
    let thread = created.json()["thread_id"]
        .as_str()
        .expect("thread")
        .to_owned();
    runner.read_commands(1);
    assert_eq!(runner.commands[0]["idempotency_key"], key.as_str());

    // The runner is full: tunnel.md 3.6 `command_result{capacity}`.
    runner.ws.send_text(
        &json!({
            "t": "command_result",
            "seq": 1,
            "event_id": unique("evt_cap"),
            "idempotency_key": key,
            "ok": false,
            "code": "capacity",
            "message": "1 sessions already running",
        })
        .to_string(),
    );

    let deadline = Instant::now() + WAIT;
    let error = loop {
        let events = Ws::thread_events(&thread);
        if let Some(error) = events
            .iter()
            .find(|event| event["event"]["type"] == "RunError")
        {
            break error.clone();
        }
        assert!(
            Instant::now() < deadline,
            "the task was not failed: {events:?}"
        );
        std::thread::sleep(Duration::from_millis(150));
    };
    assert_eq!(error["task_id"], task.as_str());
    assert_eq!(error["event"]["code"], "capacity");
    assert!(
        error["event"]["message"]
            .as_str()
            .is_some_and(|m| m.contains("capacity")),
        "{error}"
    );
}

#[test]
fn a_task_for_an_offline_machine_is_queued_and_delivered_when_it_reconnects() {
    // Plan 4.10 "Sleep and disconnects": messages sent from the phone are queued on the hub and
    // delivered in order when the runner reconnects. Creating the thread is accepted at once.
    let (machine, credential) = dev_machine();
    let runner = Runner::connect(&machine, &credential);
    wait_listed(&machine, "online", |entry| {
        entry["presence"]["state"] == "online"
    });
    drop(runner);
    let offline = wait_listed(&machine, "offline", |entry| {
        entry["presence"]["state"] == "offline"
    });
    assert!(offline["presence"]["since"].is_string());

    let first = unique("idk_off1");
    let second = unique("idk_off2");
    for key in [&first, &second] {
        let created = create(key, &create_body(&machine, "worktree"));
        assert_eq!(
            created.status, 201,
            "create while offline: {}",
            created.body
        );
        assert_eq!(created.json()["status"], "started");
    }
    assert_eq!(outbox_len(&machine), 2, "both dispatches wait on the hub");

    let mut back = Runner::connect(&machine, &credential);
    assert_eq!(back.welcome["t"], "welcome");
    back.read_commands(2);
    let keys: Vec<&str> = back
        .commands
        .iter()
        .map(|c| c["idempotency_key"].as_str().expect("key"))
        .collect();
    assert_eq!(keys, [first.as_str(), second.as_str()], "in order");
    assert!(
        back.commands
            .iter()
            .all(|c| c["command"]["workspace"]["spawn"] == "worktree")
    );
    wait_listed(&machine, "online again", |entry| {
        entry["presence"]["state"] == "online"
    });
}

#[test]
fn the_machines_list_renames_and_revokes_for_the_machines_screen() {
    let (machine, _credential) = dev_machine();
    assert!(listed(&machine).is_some());

    let path = format!("/v1/machines/{machine}");
    let renamed = http(
        "PATCH",
        &path,
        true,
        None,
        Some(&json!({ "name": "  desk at home  " })),
    );
    assert_eq!(renamed.status, 200, "rename: {}", renamed.body);
    assert_eq!(renamed.json()["name"], "desk at home");
    assert_eq!(listed(&machine).expect("listed")["name"], "desk at home");

    for body in [json!({}), json!({ "name": "   " }), json!({ "name": 4 })] {
        let bad = http("PATCH", &path, true, None, Some(&body));
        assert_eq!(bad.status, 400, "{body}: {}", bad.body);
    }
    let unknown = http(
        "PATCH",
        "/v1/machines/mch_nope",
        true,
        None,
        Some(&json!({ "name": "x" })),
    );
    assert_eq!(unknown.status, 404);
    let unauthenticated = http("PATCH", &path, false, None, Some(&json!({ "name": "x" })));
    assert_eq!(unauthenticated.status, 401);
    assert_eq!(
        listed(&machine).expect("still listed")["name"],
        "desk at home"
    );

    let revoked = http("DELETE", &path, true, Some(&unique("idk_rev")), None);
    assert_eq!(revoked.status, 200, "revoke: {}", revoked.body);
    assert!(
        listed(&machine).is_none(),
        "a revoked machine is not listed"
    );
    let after = http("PATCH", &path, true, None, Some(&json!({ "name": "x" })));
    assert_eq!(after.status, 404, "a revoked machine cannot be renamed");
    let task = create(&unique("idk_gone"), &create_body(&machine, "same_dir"));
    assert_eq!(task.status, 404, "and takes no task: {}", task.body);
}
