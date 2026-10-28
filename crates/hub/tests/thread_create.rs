//! M2-008c live tests of `POST /v1/threads` against a running hub (`wrangler dev --local`).
//!
//! Needs `HUB_URL` and a hub started with `DEV_FIXTURES=1` (`crates/hub/.dev.vars` does that for
//! `wrangler dev`). Run it through `bash scripts/hub-test.sh thread_create`, which builds the
//! Worker, starts wrangler and sets `HUB_URL`. Without `HUB_URL` every test fails with a message
//! saying so. The file is compiled only with the `live` feature (`scripts/hub-test.sh` turns it
//! on), so a plain `cargo test -p hub` skips it.
//!
//! The fake runner is a hand-written tunnel client on `std::net` (the same contract as
//! `tests/machine_do.rs`): `hello`, `welcome`, then the `command` frames the Machine DO queued for
//! the thread's `dispatch`. Every test uses its own machine; the thread id is the hub's answer.

#![cfg(feature = "live")]

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::OnceLock;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::tunnel::{Receiver, Verdict};
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
             Run it with `bash scripts/hub-test.sh thread_create`."
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
    welcome: Value,
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

/// A machine of the dev org, created through `POST /v1/machines` (so it has a D1 row, which
/// `POST /v1/threads` requires): `(machine id, credential)`.
fn dev_machine() -> (String, String) {
    machine_of(Some("dev"))
}

fn machine_of(token: Option<&str>) -> (String, String) {
    let reply = http_as(
        "POST",
        "/v1/machines",
        token,
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

/// An owner session token of a second org (the hub's one-time bootstrap; this test binary runs
/// against its own hub, so nothing else has used it).
fn foreign_owner_token() -> &'static str {
    static TOKEN: OnceLock<String> = OnceLock::new();
    TOKEN.get_or_init(|| {
        let secret = std::env::var("BOOTSTRAP_OWNER_TOKEN").unwrap_or_else(|_| {
            panic!(
                "BOOTSTRAP_OWNER_TOKEN is not set: run `bash scripts/hub-test.sh thread_create`."
            )
        });
        let reply = http_as(
            "POST",
            "/v1/auth/bootstrap",
            Some(&secret),
            None,
            Some(&json!({ "org_name": "Other Org", "user_name": "Other Owner" })),
        );
        assert_eq!(reply.status, 201, "bootstrap: {}", reply.body);
        let out = reply.json();
        assert_ne!(out["org"]["id"], "org_dev");
        out["session"]["token"].as_str().expect("token").to_owned()
    })
}

/// A machine of the second org: `(machine id, credential)`.
fn foreign_org_machine() -> (String, String) {
    machine_of(Some(foreign_owner_token()))
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

fn create_body(machine: &str, prompt: bool) -> Value {
    let mut body = json!({
        "machine_id": machine,
        "agent": "claude",
        "workspace": { "path": "/tmp/work", "spawn": "worktree" },
        "permission_mode": "default",
        "title": "Upgrade axum",
    });
    if prompt {
        body["prompt"] = json!({
            "task_id": format!("tk_{}", unique("first")),
            "message_id": format!("msg_{}", unique("first")),
            "parts": [{ "type": "text", "text": "Upgrade axum to 0.8" }],
        });
    }
    body
}

fn create(key: &str, body: &Value) -> Reply {
    http("POST", "/v1/threads", true, Some(key), Some(body))
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn creating_a_thread_queues_its_dispatch_and_the_runner_gets_it_after_welcome() {
    let (machine, credential) = dev_machine();
    let key = unique("idk_create");
    let body = create_body(&machine, true);

    let created = create(&key, &body);
    assert_eq!(created.status, 201, "create: {}", created.body);
    let answer = created.json();
    let thread = answer["thread_id"].as_str().expect("thread_id").to_owned();
    let task = answer["task_id"].as_str().expect("task_id").to_owned();
    assert!(thread.starts_with("th_"), "thread id: {thread}");
    assert_eq!(answer["status"], "started");

    // The init hook recorded the host machine; the first task's UserMessage is in the log.
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(info["machine_id"], machine.as_str());
    assert_eq!(info["title"], "Upgrade axum");
    assert_eq!(
        info["head_seq"], 2,
        "StateSnapshot (thread) and UserMessage"
    );

    // The runner connects after the create: the dispatch waits in the Machine DO's queue and
    // arrives after welcome (tunnel.md §5).
    let mut runner = Runner::connect(&machine, &credential);
    assert_eq!(runner.welcome["resumed"], false);
    runner.read_commands(1);
    let command = &runner.commands[0];
    assert_eq!(command["command"]["kind"], "dispatch", "{command}");
    assert_eq!(command["idempotency_key"], key.as_str());
    let dispatch = &command["command"];
    assert_eq!(dispatch["thread_id"], thread.as_str());
    assert_eq!(dispatch["task_id"], task.as_str());
    assert_eq!(dispatch["agent"], "claude");
    assert_eq!(dispatch["workspace"]["path"], "/tmp/work");
    assert_eq!(dispatch["permission_mode"], "default");
    assert_eq!(dispatch["parts"][0]["text"], "Upgrade axum to 0.8");
    assert_eq!(dispatch["author"]["id"], "usr_dev");
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "exactly one command for one create"
    );
}

#[test]
fn an_idempotency_key_replay_returns_the_same_response_and_no_second_thread() {
    let (machine, credential) = dev_machine();
    let key = unique("idk_replay");
    let body = create_body(&machine, true);

    let first = create(&key, &body);
    assert_eq!(first.status, 201, "create: {}", first.body);
    let replay = create(&key, &body);
    assert_eq!(replay.status, 201, "replay: {}", replay.body);
    assert_eq!(replay.json(), first.json(), "the stored answer is replayed");

    // Same thread, nothing appended by the replay, one dispatch queued.
    let thread = first.json()["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_owned();
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(info["head_seq"], 2, "the replay appended nothing");
    let mut runner = Runner::connect(&machine, &credential);
    runner.read_commands(1);
    assert_eq!(runner.commands[0]["command"]["kind"], "dispatch");
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "the replay queued no second dispatch"
    );

    // The same key with another body is refused, not a second thread.
    let mut changed = body.clone();
    changed["title"] = json!("Something else");
    assert_eq!(create(&key, &changed).status, 422);
}

#[test]
fn a_thread_created_without_a_prompt_is_idle_and_nothing_is_dispatched() {
    let (machine, credential) = dev_machine();
    let created = create(&unique("idk_idle"), &create_body(&machine, false));
    assert_eq!(created.status, 201, "create: {}", created.body);
    assert_eq!(created.json()["status"], "idle");
    assert!(created.json()["task_id"].is_null());

    let thread = created.json()["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_owned();
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(info["machine_id"], machine.as_str());
    assert_eq!(info["head_seq"], 1, "only the thread state");

    let mut runner = Runner::connect(&machine, &credential);
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "an idle thread queues no command"
    );
}

/// Asserts the 404 `not_found` of a refused create and that nothing was written for it: no
/// Thread DO state (head_seq 0), no `thread.create` audit row, nothing queued on `machines`.
fn assert_refused_and_nothing_written(machine: &str, machines: &[&str]) {
    let key = unique("idk_refused");
    let reply = create(&key, &create_body(machine, true));
    assert_eq!(reply.status, 404, "create: {}", reply.body);
    assert_eq!(reply.json()["error"], "not_found", "{}", reply.body);

    let thread = derived_thread_id(&key);
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None);
    assert!(
        info.status == 404 || info.json()["head_seq"] == 0,
        "a Thread DO was written: {} {}",
        info.status,
        info.body
    );
    let audit = http("GET", "/v1/dev/audit?limit=1000", true, None, None);
    assert_eq!(audit.status, 200, "audit: {}", audit.body);
    assert!(
        !audit.body.contains(&thread),
        "a refused create was audited: {}",
        audit.body
    );
    for id in machines {
        assert_eq!(outbox_len(id), 0, "a dispatch was queued on {id}");
    }
}

#[test]
fn a_machine_that_does_not_exist_is_404_and_nothing_is_written() {
    let machine = unique("mch_unknown");
    assert_refused_and_nothing_written(&machine, &[&machine]);
}

#[test]
fn a_revoked_machine_is_404_and_nothing_is_written() {
    let (machine, _) = dev_machine();
    let revoked = http(
        "DELETE",
        &format!("/v1/machines/{machine}"),
        true,
        None,
        None,
    );
    assert_eq!(revoked.status, 200, "revoke: {}", revoked.body);
    assert_refused_and_nothing_written(&machine, &[&machine]);
}

#[test]
fn a_machine_of_another_org_is_404_like_an_unknown_one_and_nothing_is_written() {
    let (foreign, credential) = foreign_org_machine();
    // The caller (the dev org's owner) gets the same answer as for an unknown machine.
    let unknown = create(&unique("idk_u"), &create_body(&unique("mch_none"), true));
    assert_refused_and_nothing_written(&foreign, &[&foreign]);
    let refused = create(&unique("idk_f"), &create_body(&foreign, true));
    assert_eq!(refused.status, unknown.status);
    assert_eq!(refused.json()["error"], unknown.json()["error"]);

    // The foreign machine's runner never sees a dispatch.
    let mut runner = Runner::connect(&foreign, &credential);
    assert!(
        runner.ws.recv_within(Duration::from_millis(500)).is_none(),
        "no command reached the other org's machine"
    );
}

#[test]
fn a_viewer_cannot_create_a_thread() {
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
    let reply = http_as(
        "POST",
        "/v1/threads",
        Some(&viewer),
        Some(&unique("idk_viewer")),
        Some(&create_body(&machine, true)),
    );
    assert_eq!(reply.status, 403, "viewer create: {}", reply.body);
    assert_eq!(reply.json()["error"], "forbidden");
    assert_eq!(outbox_len(&machine), 0, "nothing was queued");
}

// ---- thread routes are scoped to the caller's org (M2-008e) ---------------------------------

/// The status code of a WebSocket upgrade request to `path` as `token` (101 when it is
/// accepted; the socket is dropped right after).
fn upgrade_status(path: &str, token: &str) -> u16 {
    let mut stream = connect();
    let request = format!(
        "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {token}\r\n\
         Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
        authority()
    );
    stream.write_all(request.as_bytes()).expect("send upgrade");
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    while !line.ends_with(b"\r\n") {
        match stream.read(&mut byte) {
            Ok(1) => line.push(byte[0]),
            other => panic!("connection ended before a status line ({other:?}): {line:?}"),
        }
    }
    String::from_utf8_lossy(&line)
        .split_whitespace()
        .nth(1)
        .and_then(|status| status.parse().ok())
        .expect("status code")
}

/// `true` when `reply` is the `404 not_found` the org check answers for `thread`.
fn is_no_thread(reply: &Reply, thread: &str) -> bool {
    reply.status == 404
        && reply.json()["error"] == "not_found"
        && reply.json()["message"] == format!("no thread {thread}").as_str()
}

#[test]
fn a_member_of_another_org_gets_404_on_every_thread_route_and_the_owner_org_still_works() {
    let (machine, _credential) = dev_machine();
    let created = create(&unique("idk_scoped"), &create_body(&machine, true));
    assert_eq!(created.status, 201, "create: {}", created.body);
    let thread = created.json()["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_owned();
    let base = format!("/v1/threads/{thread}");
    let head_before = http("GET", &base, true, None, None).json()["head_seq"].clone();
    let outbox_before = outbox_len(&machine);

    let foreign = foreign_owner_token();
    let requests: Vec<(&str, String, Option<Value>)> = vec![
        ("GET", base.clone(), None),
        ("GET", format!("{base}/pending"), None),
        (
            "POST",
            format!("{base}/prompts"),
            Some(json!({
                "task_id": unique("tk_foreign"),
                "parts": [{ "type": "text", "text": "hijack" }],
            })),
        ),
        (
            "POST",
            format!("{base}/interrupts/int_foreign"),
            Some(json!({ "outcome": { "type": "selected", "option_id": "allow" } })),
        ),
        (
            "POST",
            format!("{base}/cancel"),
            Some(json!({ "task_id": "tk_foreign" })),
        ),
        (
            "POST",
            format!("{base}/session"),
            Some(json!({ "mode_id": "plan" })),
        ),
        ("DELETE", format!("{base}/pending/tk_foreign"), None),
    ];
    // The other org's calls first, so none of them can be answered from the owner's effects.
    for (method, path, body) in &requests {
        // Another org's owner: no role excuse, and a valid body, so only the org check refuses.
        let denied = http_as(
            method,
            path,
            Some(foreign),
            Some(&unique("idk_foreign")),
            body.as_ref(),
        );
        assert!(
            is_no_thread(&denied, &thread),
            "{method} {path} as the other org: {} {}",
            denied.status,
            denied.body
        );
    }
    assert_eq!(upgrade_status(&format!("{base}/subscribe"), foreign), 404);
    assert_eq!(
        http("GET", &base, true, None, None).json()["head_seq"],
        head_before,
        "nothing reached the Thread DO"
    );
    assert_eq!(
        outbox_len(&machine),
        outbox_before,
        "nothing reached the Machine DO"
    );

    // The thread's own org gets past the gate (whatever the route then answers).
    for (method, path, body) in &requests {
        let allowed = http_as(
            method,
            path,
            Some("dev"),
            Some(&unique("idk_owner")),
            body.as_ref(),
        );
        assert!(
            !is_no_thread(&allowed, &thread),
            "{method} {path} as the owning org: {} {}",
            allowed.status,
            allowed.body
        );
    }
    assert_eq!(upgrade_status(&format!("{base}/subscribe"), "dev"), 101);
}

#[test]
fn an_unknown_thread_is_404_for_every_caller() {
    let thread = unique("th_never_hosted");
    for token in ["dev", foreign_owner_token()] {
        let reply = http_as(
            "GET",
            &format!("/v1/threads/{thread}"),
            Some(token),
            None,
            None,
        );
        assert!(
            is_no_thread(&reply, &thread),
            "{} {}",
            reply.status,
            reply.body
        );
        let prompt = http_as(
            "POST",
            &format!("/v1/threads/{thread}/prompts"),
            Some(token),
            Some(&unique("idk_unknown")),
            Some(&json!({ "task_id": "tk_1", "parts": [{ "type": "text", "text": "hi" }] })),
        );
        assert!(
            is_no_thread(&prompt, &thread),
            "{} {}",
            prompt.status,
            prompt.body
        );
    }
}

#[test]
fn a_thread_already_mapped_to_another_org_cannot_be_claimed_by_a_dev_init() {
    let (machine, _) = foreign_org_machine();
    let created = http_as(
        "POST",
        "/v1/threads",
        Some(foreign_owner_token()),
        Some(&unique("idk_other_org")),
        Some(&create_body(&machine, false)),
    );
    assert_eq!(created.status, 201, "foreign create: {}", created.body);
    let thread = created.json()["thread_id"]
        .as_str()
        .expect("thread_id")
        .to_owned();
    // The dev org (another org) tries to record a host for it: refused, nothing initialised.
    for route in ["init", "host"] {
        let reply = http(
            "POST",
            &format!("/v1/dev/threads/{thread}/{route}"),
            true,
            None,
            Some(&json!({ "machine_id": "mch_dev_grab", "title": "grabbed" })),
        );
        assert!(
            is_no_thread(&reply, &thread),
            "{route}: {} {}",
            reply.status,
            reply.body
        );
    }
    // The owner org still reads it, and its title is untouched; the dev org still gets 404.
    let owned = http_as(
        "GET",
        &format!("/v1/threads/{thread}"),
        Some(foreign_owner_token()),
        None,
        None,
    );
    assert_eq!(owned.status, 200, "{}", owned.body);
    assert_eq!(owned.json()["title"], "Upgrade axum");
    let dev = http("GET", &format!("/v1/threads/{thread}"), true, None, None);
    assert!(is_no_thread(&dev, &thread), "{} {}", dev.status, dev.body);
}
