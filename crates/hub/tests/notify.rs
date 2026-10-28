//! M2-011 live tests of Web Push notifications against a running hub (`wrangler dev --local`):
//! a thread event leads, through the NOTIFY queue, to one encrypted and VAPID-signed POST at a
//! fake push service served by this test process, whose body the test decrypts with the
//! subscription's private key.
//!
//! Needs `HUB_URL` and a hub started with `DEV_FIXTURES=1` (`crates/hub/.dev.vars`) and a
//! `ATTACHMENT_SIGNING_KEY` var, from which a hub with `DEV_FIXTURES=1` and no `VAPID_PRIVATE_KEY`
//! derives a throwaway VAPID key. `bash scripts/hub-test.sh notify` does all of that. Without
//! `HUB_URL` every test fails with a message saying so; nothing passes vacuously. The `live`
//! feature (required by `[[test]] notify`) keeps this file out of a plain `cargo test -p hub`.
//!
//! All tests act as the dev user, and every thread event of the dev org notifies all of that
//! user's subscriptions, so the tests run one at a time (`serial()`) and remove their
//! subscriptions and prefs when they start.

use std::collections::VecDeque;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use hub::notify::webpush::{b64_decode, b64_encode, decrypt};
use p256::SecretKey;
use p256::ecdsa::signature::Verifier;
use p256::ecdsa::{Signature, VerifyingKey};
use p256::elliptic_curve::sec1::ToEncodedPoint;
use protocol::turn_event::format_timestamp;
use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// How long to wait for a push that must arrive (the queue batches for about a second).
const PUSH_WAIT: Duration = Duration::from_secs(25);
/// How long to wait to be sure a push does not arrive.
const QUIET_WAIT: Duration = Duration::from_secs(4);
const SECRET_TITLE: &str = "Run rm -rf /home/ada/secret-plans.txt?";

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
             Run it with `bash scripts/hub-test.sh notify`."
        ),
    };
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

/// One request with `Connection: close`; `auth` adds `Authorization: Bearer dev`.
fn http(
    method: &str,
    path: &str,
    auth: bool,
    idempotency_key: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    let authority = authority();
    let mut stream = TcpStream::connect(&authority)
        .unwrap_or_else(|error| panic!("cannot connect to HUB_URL {authority}: {error}"));
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    let mut request =
        format!("{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
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
    let body = if head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked")
    {
        String::from_utf8_lossy(&dechunk(body.as_bytes())).into_owned()
    } else {
        body.to_owned()
    };
    Reply { status, body }
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
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

/// One request the fake push service received.
#[derive(Clone)]
struct Received {
    path: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> &str {
        self.headers
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, v)| v.as_str())
            .unwrap_or_else(|| panic!("push request has no {name} header: {:?}", self.headers))
    }
}

/// A push service on a loopback port: records every POST and answers with the next status of
/// `script`, else 201.
struct PushService {
    port: u16,
    received: Arc<Mutex<Vec<Received>>>,
    script: Arc<Mutex<VecDeque<u16>>>,
}

impl PushService {
    fn start() -> Self {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind fake push service");
        let port = listener.local_addr().expect("addr").port();
        let received = Arc::new(Mutex::new(Vec::new()));
        let script = Arc::new(Mutex::new(VecDeque::new()));
        let (log, answers) = (Arc::clone(&received), Arc::clone(&script));
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let (log, answers) = (Arc::clone(&log), Arc::clone(&answers));
                std::thread::spawn(move || serve(stream, &log, &answers));
            }
        });
        Self {
            port,
            received,
            script,
        }
    }

    fn endpoint(&self, name: &str) -> String {
        format!("http://127.0.0.1:{}/push/{name}", self.port)
    }

    fn answer_next(&self, statuses: &[u16]) {
        self.script.lock().unwrap().extend(statuses.iter().copied());
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

fn serve(mut stream: TcpStream, log: &Mutex<Vec<Received>>, answers: &Mutex<VecDeque<u16>>) {
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
    let mut lines = head.split("\r\n");
    let request_line = lines.next().unwrap_or_default();
    let path = request_line
        .split_whitespace()
        .nth(1)
        .unwrap_or_default()
        .to_owned();
    let headers: Vec<(String, String)> = lines
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
        body = dechunk(&body);
    }
    log.lock().unwrap().push(Received {
        path,
        headers,
        body,
    });
    let status = answers.lock().unwrap().pop_front().unwrap_or(201);
    let reply = format!("HTTP/1.1 {status} X\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
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

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn unique(name: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    format!(
        "{name}_{}_{:x}_{}",
        std::process::id(),
        now_ms(),
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// A thread mapped to the dev org (as thread creation would).
fn unique_thread(name: &str) -> String {
    let thread = format!("th_{}", unique(name));
    let hosted = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/host"),
        true,
        None,
        Some(&json!({ "machine_id": "mch_notify" })),
    );
    assert_eq!(hosted.status, 200, "host thread: {}", hosted.body);
    thread
}

fn runner_event(thread: &str, task: &str, event: Value) {
    let envelope = json!({
        "v": 1, "seq": 0, "thread_id": thread, "task_id": task, "run_id": "run_1",
        "ts": format_timestamp(now_ms()), "event": event,
    });
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/events"),
        true,
        None,
        Some(&envelope),
    );
    assert_eq!(reply.status, 200, "dev append: {}", reply.body);
}

/// A prompt, which opens the run, then `RunStarted`.
fn start_run(thread: &str, task: &str) {
    let reply = http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        true,
        Some(&unique("idk")),
        Some(
            &json!({ "task_id": task, "parts": [{ "type": "text", "text": "do the secret thing" }] }),
        ),
    );
    assert_eq!(reply.status, 202, "prompt: {}", reply.body);
    runner_event(
        thread,
        task,
        json!({ "type": "RunStarted", "reason": "prompt" }),
    );
}

fn raise_interrupt(thread: &str, task: &str, interrupt: &str) {
    runner_event(
        thread,
        task,
        json!({
            "type": "Interrupt", "interrupt_id": interrupt, "kind": "approval",
            "title": SECRET_TITLE, "message": "it will read secret-plans.txt",
            "expires_at": format_timestamp(now_ms() + 10 * 60 * 1000),
            "options": [
                { "option_id": "allow", "name": "Allow", "kind": "allow_once" },
                { "option_id": "reject", "name": "Reject", "kind": "reject_once" },
            ],
        }),
    );
}

fn finish_run(thread: &str, task: &str) {
    runner_event(
        thread,
        task,
        json!({ "type": "RunFinished", "outcome": { "type": "success", "stop_reason": "end_turn" } }),
    );
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

fn reset_prefs() {
    let reply = http("PUT", "/v1/push/prefs", true, None, Some(&json!({})));
    assert_eq!(reply.status, 200, "reset prefs: {}", reply.body);
}

/// A fresh service, a fresh browser subscribed to it, default prefs.
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
fn an_interrupt_leads_to_one_encrypted_vapid_signed_push_with_ids_only() {
    let _serial = serial();
    let (service, browser, endpoint) = setup("interrupt");
    let thread = unique_thread("int");
    let task = "tk_notify_1";

    start_run(&thread, task);
    raise_interrupt(&thread, task, "int_1");
    let request = service.wait_for(0);

    // Web Push headers (RFC 8030, 8291, 8292).
    assert_eq!(request.path, "/push/interrupt");
    assert_eq!(request.header("content-encoding"), "aes128gcm");
    assert_eq!(
        request.header("topic"),
        task,
        "one pending notification per task"
    );
    assert_eq!(request.header("urgency"), "high");
    assert_eq!(request.header("ttl"), "86400");
    let authorization = request.header("authorization");
    let rest = authorization
        .strip_prefix("vapid t=")
        .expect("vapid scheme");
    let (jwt, k) = rest.split_once(", k=").expect("k parameter");
    let parts: Vec<&str> = jwt.split('.').collect();
    let claims: Value = serde_json::from_slice(&b64_decode(parts[1]).unwrap()).unwrap();
    assert_eq!(
        claims["aud"],
        format!("http://127.0.0.1:{}", service.port),
        "aud is the push service origin"
    );
    let verifying =
        VerifyingKey::from_sec1_bytes(&b64_decode(k).unwrap()).expect("k is a P-256 key");
    verifying
        .verify(
            format!("{}.{}", parts[0], parts[1]).as_bytes(),
            &Signature::from_slice(&b64_decode(parts[2]).unwrap()).unwrap(),
        )
        .expect("the VAPID JWT is signed by k");
    // The hub serves that same key to clients that subscribe.
    let served = http("GET", "/v1/push/vapid-public-key", true, None, None);
    assert_eq!(served.status, 200, "{}", served.body);
    assert_eq!(served.json()["public_key"], k);

    // The body is ciphertext; the browser's key opens it, and it holds ids and a generic title.
    let raw = String::from_utf8_lossy(&request.body).into_owned();
    assert!(!raw.contains(&thread) && !raw.contains("int_1"));
    let payload = browser.open(&request);
    assert_eq!(payload["kind"], "approval_needed");
    assert_eq!(payload["title"], "Approval needed");
    assert_eq!(payload["thread_id"], thread.as_str());
    assert_eq!(payload["task_id"], task);
    assert_eq!(payload["interrupt_id"], "int_1");
    let text = payload.to_string();
    for leaked in ["rm -rf", "secret", "do the"] {
        assert!(!text.contains(leaked), "payload leaks {leaked:?}: {text}");
    }

    // Actionable approval: the action answers through the existing, authenticated route.
    let allow = payload["actions"]
        .as_array()
        .and_then(|a| a.iter().find(|a| a["action"] == "allow"))
        .expect("allow action");
    assert_eq!(payload["actions"][1]["action"], "deny");
    let answer = http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_1"),
        true,
        Some(&unique("idk")),
        Some(&json!({ "outcome": { "type": "selected", "option_id": allow["option_id"] } })),
    );
    assert_eq!(
        answer.status, 200,
        "answer from the notification: {}",
        answer.body
    );
    assert_eq!(answer.json()["interrupt_id"], "int_1");
    let unauthenticated = http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_1"),
        false,
        Some(&unique("idk")),
        Some(&json!({ "outcome": { "type": "selected", "option_id": "allow" } })),
    );
    assert_eq!(unauthenticated.status, 401, "no new unauthenticated route");

    service.assert_no_more_than(1);
    unsubscribe(&endpoint);
}

#[test]
fn a_gone_endpoint_is_deleted_and_a_5xx_is_retried_by_the_queue() {
    let _serial = serial();
    let (service, _browser, endpoint) = setup("flaky");

    // 503 first: the consumer asks the queue to retry, and the retry is delivered.
    service.answer_next(&[503]);
    let thread = unique_thread("retry");
    start_run(&thread, "tk_notify_2");
    finish_run(&thread, "tk_notify_2");
    let first = service.wait_for(0);
    let retried = service.wait_for(1);
    assert_eq!(first.path, retried.path);
    assert_eq!(retried.header("topic"), "tk_notify_2");
    service.assert_no_more_than(2);

    // 410: the subscription is deleted. Registering the endpoint again creates a new row (201),
    // where an existing row would answer 200.
    service.answer_next(&[410]);
    let thread = unique_thread("gone");
    start_run(&thread, "tk_notify_3");
    finish_run(&thread, "tk_notify_3");
    service.wait_for(2);
    service.assert_no_more_than(3);
    let again = subscribe(&Browser::new(), &endpoint);
    assert_eq!(
        again.status, 201,
        "the 410 deleted the subscription: {}",
        again.body
    );

    // 404 behaves the same.
    service.answer_next(&[404]);
    let thread = unique_thread("gone404");
    start_run(&thread, "tk_notify_4");
    finish_run(&thread, "tk_notify_4");
    service.wait_for(3);
    service.assert_no_more_than(4);
    let again = subscribe(&Browser::new(), &endpoint);
    assert_eq!(
        again.status, 201,
        "the 404 deleted the subscription: {}",
        again.body
    );
    unsubscribe(&endpoint);
}

#[test]
fn prefs_class_thread_mute_and_quiet_hours_suppress_deliveries() {
    let _serial = serial();
    let (service, browser, endpoint) = setup("prefs");
    let (muted, open) = (unique_thread("muted"), unique_thread("open"));

    let prefs = http(
        "PUT",
        "/v1/push/prefs",
        true,
        None,
        Some(&json!({
            "classes": { "task_failed": false },
            "muted_threads": [muted],
        })),
    );
    assert_eq!(prefs.status, 200, "put prefs: {}", prefs.body);
    assert_eq!(
        http("GET", "/v1/push/prefs", true, None, None).json(),
        prefs.json()
    );

    // A muted thread and a switched-off class are both skipped.
    start_run(&muted, "tk_muted");
    finish_run(&muted, "tk_muted");
    start_run(&open, "tk_failed");
    runner_event(
        &open,
        "tk_failed",
        json!({ "type": "RunError", "reason": "error", "message": "boom" }),
    );
    service.assert_no_more_than(0);

    // The same open thread still notifies for a class that is on.
    start_run(&open, "tk_done");
    finish_run(&open, "tk_done");
    let request = service.wait_for(0);
    let payload = browser.open(&request);
    assert_eq!(payload["kind"], "task_done");
    assert_eq!(payload["title"], "Task finished");
    assert_eq!(request.header("urgency"), "normal");
    service.assert_no_more_than(1);

    // Quiet hours around now (in UTC) skip everything; a window elsewhere does not.
    let minute_now = (now_ms() / 60_000) % (24 * 60);
    let hhmm = |minute: u64| format!("{:02}:{:02}", (minute / 60) % 24, minute % 60);
    let around_now = json!({ "quiet_hours": {
        "start": hhmm(minute_now + 24 * 60 - 120), "end": hhmm(minute_now + 120), "tz": "UTC" } });
    assert_eq!(
        http("PUT", "/v1/push/prefs", true, None, Some(&around_now)).status,
        200
    );
    start_run(&open, "tk_quiet");
    finish_run(&open, "tk_quiet");
    service.assert_no_more_than(1);
    let elsewhere = json!({ "quiet_hours": {
        "start": hhmm(minute_now + 240), "end": hhmm(minute_now + 300), "tz": "UTC" } });
    assert_eq!(
        http("PUT", "/v1/push/prefs", true, None, Some(&elsewhere)).status,
        200
    );
    start_run(&open, "tk_loud");
    finish_run(&open, "tk_loud");
    service.wait_for(1);

    reset_prefs();
    unsubscribe(&endpoint);
}

#[test]
fn the_push_routes_validate_and_authenticate() {
    let _serial = serial();
    reset_prefs();
    let browser = Browser::new();
    let service = PushService::start();
    let endpoint = service.endpoint("routes");

    // 401 without a token, on every route.
    for (method, path) in [
        ("POST", "/v1/push/subscriptions"),
        ("DELETE", "/v1/push/subscriptions"),
        ("GET", "/v1/push/prefs"),
        ("PUT", "/v1/push/prefs"),
        ("GET", "/v1/push/vapid-public-key"),
    ] {
        let body = json!({});
        let reply = http(method, path, false, None, Some(&body));
        assert_eq!(reply.status, 401, "{method} {path}: {}", reply.body);
    }

    // Idempotent per endpoint: 201 then 200 with the same id.
    let first = subscribe(&browser, &endpoint);
    assert_eq!(first.status, 201, "{}", first.body);
    let again = subscribe(&browser, &endpoint);
    assert_eq!(again.status, 200, "{}", again.body);
    assert_eq!(first.json()["id"], again.json()["id"]);

    // Bad input is 400 in the client-api error shape.
    for body in [
        json!({}),
        json!({ "endpoint": endpoint }),
        json!({ "endpoint": "http://example.com/x", "keys": browser.subscription("x")["keys"] }),
        json!({ "endpoint": "https://push.example.net/x", "keys": { "p256dh": "AAAA", "auth": "AAAA" } }),
    ] {
        let reply = http("POST", "/v1/push/subscriptions", true, None, Some(&body));
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
        assert_eq!(reply.json()["error"], "invalid_request");
    }
    for body in [
        json!({ "classes": { "nope": true } }),
        json!({ "quiet_hours": { "start": "22:00", "end": "07:00", "tz": "Mars/Base" } }),
        json!({ "quiet_hours": { "start": "22:00", "end": "22:00", "tz": "UTC" } }),
    ] {
        let reply = http("PUT", "/v1/push/prefs", true, None, Some(&body));
        assert_eq!(reply.status, 400, "{body}: {}", reply.body);
    }
    // An IANA zone resolves on the Worker (the runtime's tz database).
    let berlin =
        json!({ "quiet_hours": { "start": "22:00", "end": "07:00", "tz": "Europe/Berlin" } });
    let reply = http("PUT", "/v1/push/prefs", true, None, Some(&berlin));
    assert_eq!(reply.status, 200, "{}", reply.body);
    assert_eq!(reply.json()["quiet_hours"]["tz"], "Europe/Berlin");

    // DELETE removes the subscription (a re-subscribe is new again) and is safe to repeat.
    unsubscribe(&endpoint);
    unsubscribe(&endpoint);
    let fresh = subscribe(&browser, &endpoint);
    assert_eq!(fresh.status, 201, "{}", fresh.body);
    unsubscribe(&endpoint);
    reset_prefs();
}
