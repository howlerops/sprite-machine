//! M2-008 live tests of the Thread Durable Object against a running hub (`wrangler dev --local`).
//!
//! Needs `HUB_URL` (for example `http://127.0.0.1:8787`) and a hub started with
//! `DEV_FIXTURES=1` (`crates/hub/.dev.vars` does that for `wrangler dev`). Run it through
//! `bash scripts/hub-test.sh thread_do`, which builds the Worker, starts wrangler and sets
//! `HUB_URL`. Without `HUB_URL` every test fails with a message saying so; nothing passes
//! vacuously. The `live` feature (required by `[[test]] thread_do` in Cargo.toml) keeps this file
//! out of a plain `cargo test -p hub`.
//!
//! The HTTP and WebSocket clients are a few lines of `std::net` so the hub crate needs no
//! HTTP or WebSocket dependency. Every test uses its own thread id (one Thread DO each).

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

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
             Run it with `bash scripts/hub-test.sh thread_do`."
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

struct Ws {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    fn connect(path: &str) -> Self {
        let mut stream = connect();
        let request = format!(
            "GET {path} HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer dev\r\n\
             Upgrade: websocket\r\nConnection: Upgrade\r\n\
             Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
            authority()
        );
        stream.write_all(request.as_bytes()).expect("send upgrade");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            let n = stream.read(&mut byte).expect("read upgrade response");
            assert!(n == 1, "connection closed during the upgrade: {head:?}");
            head.push(byte[0]);
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        assert!(
            head.starts_with("HTTP/1.1 101"),
            "expected 101 Switching Protocols, got: {head}"
        );
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

    /// The next text frame as JSON; panics when none arrives in time.
    fn next(&mut self) -> Value {
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                match opcode {
                    0x1 => {
                        let text = String::from_utf8_lossy(&payload).into_owned();
                        return serde_json::from_str(&text)
                            .unwrap_or_else(|error| panic!("frame is not JSON ({error}): {text}"));
                    }
                    0x8 => panic!("the server closed the WebSocket"),
                    _ => continue, // ping / pong / binary: ignore
                }
            }
            let mut chunk = [0u8; 8192];
            let n = self
                .stream
                .read(&mut chunk)
                .unwrap_or_else(|error| panic!("no WebSocket message in time: {error}"));
            assert!(n > 0, "the WebSocket connection closed");
            self.buf.extend_from_slice(&chunk[..n]);
        }
    }

    /// `hello`, `snapshot`, the replayed events and `live`, as read from a fresh connection.
    fn catch_up(&mut self) -> Catchup {
        let hello = self.next();
        assert_eq!(hello["type"], "hello", "first message: {hello}");
        let snapshot = self.next();
        assert_eq!(snapshot["type"], "snapshot", "second message: {snapshot}");
        let mut events = Vec::new();
        loop {
            let message = self.next();
            match message["type"].as_str() {
                Some("event") => events.push(message["envelope"].clone()),
                Some("live") => break,
                other => panic!("unexpected message {other:?} during catch-up: {message}"),
            }
        }
        Catchup {
            hello,
            snapshot,
            events,
        }
    }

    /// The next `n` live events (envelopes).
    fn events(&mut self, n: usize) -> Vec<Value> {
        (0..n)
            .map(|_| {
                let message = self.next();
                assert_eq!(message["type"], "event", "live message: {message}");
                message["envelope"].clone()
            })
            .collect()
    }
}

struct Catchup {
    hello: Value,
    snapshot: Value,
    events: Vec<Value>,
}

impl Catchup {
    fn seqs(&self) -> Vec<u64> {
        self.events
            .iter()
            .filter_map(|e| e["seq"].as_u64())
            .collect()
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

// ---- helpers ----------------------------------------------------------------------------

fn unique_thread(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("th_{name}_{}_{nanos:x}", std::process::id())
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn envelope(thread: &str, task: &str, run: Option<&str>, event: Value) -> Value {
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

/// A runner-side event through the dev route (`seq: 0` in, stamped out).
fn runner_event(thread: &str, task: &str, run: Option<&str>, event: Value) -> Value {
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        true,
        None,
        Some(&envelope(thread, task, run, event)),
    );
    assert_eq!(reply.status, 200, "dev append: {}", reply.body);
    reply.json()["stored"][0].clone()
}

fn prompt(thread: &str, task: &str, key: &str, text: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        true,
        Some(key),
        Some(&json!({
            "task_id": task,
            "parts": [{ "type": "text", "text": text }],
        })),
    )
}

fn started() -> Value {
    json!({ "type": "RunStarted", "reason": "prompt" })
}

fn text_start() -> Value {
    json!({ "type": "TextMessageStart", "message_id": "m1", "role": "assistant" })
}

fn text_content(delta: &str) -> Value {
    json!({ "type": "TextMessageContent", "message_id": "m1", "delta": delta })
}

fn text_end() -> Value {
    json!({ "type": "TextMessageEnd", "message_id": "m1" })
}

fn finished() -> Value {
    json!({ "type": "RunFinished", "outcome": { "type": "success", "stop_reason": "end_turn" } })
}

fn types(events: &[Value]) -> Vec<&str> {
    events
        .iter()
        .filter_map(|e| e["event"]["type"].as_str())
        .collect()
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn three_subscribers_see_identical_sequences() {
    let thread = unique_thread("three");
    let path = format!("/v1/threads/{thread}/subscribe");
    let mut clients: Vec<Ws> = (0..3).map(|_| Ws::connect(&path)).collect();
    for client in &mut clients {
        let caught_up = client.catch_up();
        assert_eq!(caught_up.hello["head_seq"], 0);
        assert_eq!(caught_up.hello["replay_from"], 1);
        assert!(caught_up.events.is_empty());
    }

    // The hub emits UserMessage (seq 1); the runner side follows through the dev route.
    let reply = prompt(&thread, "tk_1", "idk_three_1", "hello");
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);
    assert_eq!(reply.json()["status"], "started");
    runner_event(&thread, "tk_1", Some("run_1"), started());
    runner_event(&thread, "tk_1", Some("run_1"), text_start());
    runner_event(
        &thread,
        "tk_1",
        Some("run_1"),
        text_content("Adding the route."),
    );
    runner_event(&thread, "tk_1", Some("run_1"), text_end());
    runner_event(&thread, "tk_1", Some("run_1"), finished());

    let mut received: Vec<Vec<Value>> = Vec::new();
    for client in &mut clients {
        received.push(client.events(6));
    }
    let seqs: Vec<u64> = received[0]
        .iter()
        .filter_map(|e| e["seq"].as_u64())
        .collect();
    assert_eq!(seqs, [1, 2, 3, 4, 5, 6], "gap-free seq assigned by the DO");
    assert_eq!(
        types(&received[0]),
        [
            "UserMessage",
            "RunStarted",
            "TextMessageStart",
            "TextMessageContent",
            "TextMessageEnd",
            "RunFinished"
        ]
    );
    assert_eq!(
        received[0], received[1],
        "client 1 and 2 see the same events"
    );
    assert_eq!(
        received[0], received[2],
        "client 1 and 3 see the same events"
    );
    assert_eq!(received[0][0]["event"]["author"]["id"], "usr_dev");

    // Ping is answered with pong (by the runtime's auto-response, without waking the DO).
    clients[0].send_text(r#"{"type":"ping"}"#);
    assert_eq!(clients[0].next(), json!({ "type": "pong" }));

    // A late subscriber with ?after=0 replays the same log.
    let mut late = Ws::connect(&path);
    let caught_up = late.catch_up();
    assert_eq!(caught_up.seqs(), [1, 2, 3, 4, 5, 6]);
    assert_eq!(caught_up.events, received[0]);
}

#[test]
fn after_in_the_middle_of_a_run_catches_up_from_the_run_boundary() {
    let thread = unique_thread("after");
    let usage = json!({
        "v": 1,
        "seq": 0,
        "thread_id": thread,
        "task_id": null,
        "run_id": null,
        "ts": format_timestamp(now_ms()),
        "event": {
            "type": "StateSnapshot",
            "scope": "usage",
            "snapshot": { "context_used": 5, "context_size": 100, "cost": null },
        },
    });
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        true,
        None,
        Some(&usage),
    );
    assert_eq!(reply.status, 200, "usage snapshot: {}", reply.body); // seq 1
    assert_eq!(prompt(&thread, "tk_1", "idk_after_1", "go").status, 202); // seq 2
    runner_event(&thread, "tk_1", Some("run_1"), started()); // 3
    runner_event(&thread, "tk_1", Some("run_1"), text_start()); // 4
    runner_event(&thread, "tk_1", Some("run_1"), text_content("par")); // 5

    // Mid-run: replay starts at the RunStarted (seq 3); the snapshot is as of seq 2.
    let mut mid = Ws::connect(&format!("/v1/threads/{thread}/subscribe?after=5"));
    let caught_up = mid.catch_up();
    assert_eq!(caught_up.hello["head_seq"], 5);
    assert_eq!(caught_up.hello["replay_from"], 3);
    assert_eq!(caught_up.snapshot["at_seq"], 2);
    assert_eq!(caught_up.snapshot["states"]["usage"]["context_used"], 5);
    assert_eq!(caught_up.seqs(), [3, 4, 5]);

    // The same client then gets the live events.
    runner_event(&thread, "tk_1", Some("run_1"), text_content("tial"));
    runner_event(&thread, "tk_1", Some("run_1"), text_end());
    runner_event(&thread, "tk_1", Some("run_1"), finished());
    let live = mid.events(3);
    let seqs: Vec<u64> = live.iter().filter_map(|e| e["seq"].as_u64()).collect();
    assert_eq!(seqs, [6, 7, 8]);

    // After the run ended nothing needs replaying.
    let mut done = Ws::connect(&format!("/v1/threads/{thread}/subscribe?after=8"));
    let caught_up = done.catch_up();
    assert_eq!(caught_up.hello["replay_from"], 9);
    assert!(caught_up.events.is_empty());
    // A reconnect that already has seq 4 (still inside run_1) replays from 3 again.
    let mut again = Ws::connect(&format!("/v1/threads/{thread}/subscribe?after=4"));
    assert_eq!(again.catch_up().hello["replay_from"], 3);
}

#[test]
fn interrupt_first_answer_is_200_and_a_later_one_is_409_naming_the_resolver() {
    let thread = unique_thread("int");
    let mut watcher = Ws::connect(&format!("/v1/threads/{thread}/subscribe"));
    watcher.catch_up();

    assert_eq!(prompt(&thread, "tk_1", "idk_int_1", "do it").status, 202);
    runner_event(&thread, "tk_1", Some("run_1"), started());
    let expires = format_timestamp(now_ms() + 10 * 60 * 1000);
    runner_event(
        &thread,
        "tk_1",
        Some("run_1"),
        json!({
            "type": "Interrupt",
            "interrupt_id": "int_1",
            "kind": "approval",
            "title": "Run cargo test?",
            "expires_at": expires,
            "options": [
                { "option_id": "allow", "name": "Allow", "kind": "allow_once" },
                { "option_id": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }),
    );
    runner_event(
        &thread,
        "tk_1",
        Some("run_1"),
        json!({
            "type": "RunFinished",
            "outcome": { "type": "interrupt", "interrupt_ids": ["int_1"] },
        }),
    );
    let path = format!("/v1/threads/{thread}/interrupts/int_1");
    let allow = json!({ "outcome": { "type": "selected", "option_id": "allow" } });

    let first = http("POST", &path, true, Some("idk_int_a"), Some(&allow));
    assert_eq!(first.status, 200, "first answer: {}", first.body);
    assert_eq!(first.json()["interrupt_id"], "int_1");
    assert_eq!(first.json()["resolved_by"]["id"], "usr_dev");

    let second = http("POST", &path, true, Some("idk_int_b"), Some(&allow));
    assert_eq!(second.status, 409, "second answer: {}", second.body);
    assert_eq!(second.json()["error"], "already_resolved");
    assert_eq!(second.json()["resolved_by"]["id"], "usr_dev");
    assert_eq!(second.json()["resolved_by"]["device_id"], "dev_dev");

    // The winner retrying with its own key gets its 200 back.
    let retry = http("POST", &path, true, Some("idk_int_a"), Some(&allow));
    assert_eq!(retry.status, 200);
    assert_eq!(retry.json(), first.json());

    // Every subscriber sees exactly one InterruptResolved, appended by the hub.
    let events = watcher.events(5); // UserMessage, RunStarted, Interrupt, RunFinished, InterruptResolved
    let resolved: Vec<&Value> = events
        .iter()
        .filter(|e| e["event"]["type"] == "InterruptResolved")
        .collect();
    assert_eq!(resolved.len(), 1, "events: {events:?}");
    assert_eq!(resolved[0]["event"]["resolved_by"]["id"], "usr_dev");
    assert_eq!(resolved[0]["event"]["outcome"]["option_id"], "allow");
    assert_eq!(resolved[0]["task_id"], "tk_1");

    // Unknown interrupt: 404 in the client-api error shape.
    let unknown = http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_9"),
        true,
        Some("idk_int_c"),
        Some(&allow),
    );
    assert_eq!(unknown.status, 404);
    assert_eq!(unknown.json()["error"], "not_found");
}

#[test]
fn pending_tasks_can_be_added_listed_and_removed_while_a_task_runs() {
    let thread = unique_thread("pending");
    let mut watcher = Ws::connect(&format!("/v1/threads/{thread}/subscribe"));
    watcher.catch_up();

    assert_eq!(prompt(&thread, "tk_1", "idk_p_1", "first").status, 202);
    runner_event(&thread, "tk_1", Some("run_1"), started());

    let second = prompt(&thread, "tk_2", "idk_p_2", "second");
    assert_eq!(second.status, 202, "{}", second.body);
    assert_eq!(
        second.json(),
        json!({ "task_id": "tk_2", "status": "pending", "position": 1 })
    );
    let third = prompt(&thread, "tk_3", "idk_p_3", "third");
    assert_eq!(third.json()["position"], 2);

    let pending = http(
        "GET",
        &format!("/v1/threads/{thread}/pending"),
        true,
        None,
        None,
    );
    assert_eq!(pending.status, 200);
    let list = pending.json()["pending"].clone();
    let ids: Vec<&str> = list
        .as_array()
        .expect("array")
        .iter()
        .filter_map(|p| p["task_id"].as_str())
        .collect();
    assert_eq!(ids, ["tk_2", "tk_3"]);
    assert_eq!(list[0]["preview"], "second");
    assert_eq!(list[0]["author"]["id"], "usr_dev");

    // The thread state (what a new subscriber's snapshot shows) lists the queue.
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(
        info["states"]["thread"]["pending_tasks"]
            .as_array()
            .map(Vec::len),
        Some(2)
    );

    // Remove tk_3: 204; removing it again is not a pending task any more.
    let removed = http(
        "DELETE",
        &format!("/v1/threads/{thread}/pending/tk_3"),
        true,
        Some("idk_p_del"),
        None,
    );
    assert_eq!(removed.status, 204, "{}", removed.body);
    let again = http(
        "DELETE",
        &format!("/v1/threads/{thread}/pending/tk_3"),
        true,
        Some("idk_p_del2"),
        None,
    );
    assert_eq!(again.status, 404);
    let pending = http(
        "GET",
        &format!("/v1/threads/{thread}/pending"),
        true,
        None,
        None,
    )
    .json();
    assert_eq!(pending["pending"].as_array().map(Vec::len), Some(1));

    // The running task ends: tk_2 starts by itself, in order, with a hub-emitted UserMessage.
    runner_event(&thread, "tk_1", Some("run_1"), finished());
    let events = watcher.events(8);
    let user_messages: Vec<&str> = events
        .iter()
        .filter(|e| e["event"]["type"] == "UserMessage")
        .filter_map(|e| e["task_id"].as_str())
        .collect();
    assert_eq!(
        user_messages,
        ["tk_1", "tk_2"],
        "events: {:?}",
        types(&events)
    );
    let pending = http(
        "GET",
        &format!("/v1/threads/{thread}/pending"),
        true,
        None,
        None,
    )
    .json();
    assert_eq!(pending["pending"], json!([]));
    // tk_2 started: it can no longer be removed from the queue.
    let started = http(
        "DELETE",
        &format!("/v1/threads/{thread}/pending/tk_2"),
        true,
        Some("idk_p_del3"),
        None,
    );
    assert_eq!(started.status, 409);
    assert_eq!(started.json()["error"], "task_started");
}

#[test]
fn idempotency_key_replays_the_same_response_without_a_new_effect() {
    let thread = unique_thread("idem");
    let first = prompt(&thread, "tk_1", "idk_same", "hello");
    assert_eq!(first.status, 202, "{}", first.body);

    let replay = prompt(&thread, "tk_1", "idk_same", "hello");
    assert_eq!((replay.status, replay.json()), (first.status, first.json()));
    let info = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(info["head_seq"], 1, "the replay appended nothing: {info}");

    // Same key, different body: 422.
    let mismatch = prompt(&thread, "tk_2", "idk_same", "something else");
    assert_eq!(mismatch.status, 422, "{}", mismatch.body);
    assert_eq!(mismatch.json()["error"], "idempotency_mismatch");

    // A write without the header is a 400.
    let missing = http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        true,
        None,
        Some(&json!({ "parts": [{ "type": "text", "text": "x" }] })),
    );
    assert_eq!(missing.status, 400);
    assert_eq!(missing.json()["error"], "idempotency_key_required");
}

#[test]
fn routes_require_the_dev_token_and_invalid_sequences_are_422() {
    let thread = unique_thread("auth");
    let no_token = http(
        "GET",
        &format!("/v1/threads/{thread}/pending"),
        false,
        None,
        None,
    );
    assert_eq!(no_token.status, 401, "{}", no_token.body);
    assert_eq!(no_token.json()["error"], "unauthorized");

    // A thread nothing happened to is a 404.
    let unknown = http("GET", &format!("/v1/threads/{thread}"), true, None, None);
    assert_eq!(unknown.status, 404);

    // RunStarted without a task is a §5 violation: 422, and nothing is stored.
    let bad = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        true,
        None,
        Some(&envelope(&thread, "tk_1", Some("run_1"), started())),
    );
    assert_eq!(bad.status, 422, "{}", bad.body);
    assert_eq!(bad.json()["error"], "invalid_sequence");
    assert_eq!(
        http("GET", &format!("/v1/threads/{thread}"), true, None, None).status,
        404
    );
}

#[test]
fn a_repeated_event_id_is_acknowledged_without_a_second_append() {
    let thread = unique_thread("event_id");
    let path = format!("/v1/threads/{thread}/subscribe");
    let mut client = Ws::connect(&path);
    assert!(client.catch_up().events.is_empty());

    // The Machine DO's shape for an event frame: `{event_id, envelope}` (tunnel.md §3.3).
    let prompted = prompt(&thread, "tk_1", "idk_event_id_1", "hello");
    assert_eq!(prompted.status, 202, "prompt: {}", prompted.body);
    let _user_message = client.events(1);
    let frame = |event_id: &str, event: Value| {
        json!({
            "event_id": event_id,
            "envelope": envelope(&thread, "tk_1", Some("run_1"), event),
        })
    };
    let route = format!("/v1/dev/threads/{thread}/events");

    let first = http(
        "POST",
        &route,
        true,
        None,
        Some(&frame("evt_dedupe_1", started())),
    );
    assert_eq!(first.status, 200, "{}", first.body);
    assert_eq!(first.json()["stored"][0]["seq"], 2);

    // The retransmit: acknowledged, nothing appended, nothing broadcast.
    let again = http(
        "POST",
        &route,
        true,
        None,
        Some(&frame("evt_dedupe_1", started())),
    );
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(again.json(), json!({ "stored": [], "duplicate": true }));
    let head = http("GET", &format!("/v1/threads/{thread}"), true, None, None).json();
    assert_eq!(head["head_seq"], 2, "the retransmit appended nothing");

    // A new event_id is a new event.
    let fresh = http(
        "POST",
        &route,
        true,
        None,
        Some(&frame("evt_dedupe_2", text_start())),
    );
    assert_eq!(fresh.status, 200, "{}", fresh.body);
    assert_eq!(fresh.json()["stored"][0]["seq"], 3);
    assert_eq!(
        client.events(2)[1]["seq"],
        3,
        "the subscriber sees one RunStarted and one text event"
    );
}
