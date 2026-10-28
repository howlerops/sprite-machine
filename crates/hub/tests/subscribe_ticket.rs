//! M2-008h live tests of the subscribe ticket (SPEC-006) against a running hub
//! (`wrangler dev --local`, `DEV_FIXTURES=1`): `POST /v1/threads/{id}/subscribe-ticket`, then a
//! real WebSocket upgrade on `GET /v1/threads/{id}/subscribe?ticket=` with no Authorization
//! header. Expiry, the exact race and the revoked-device cases are native tests
//! (`auth::tests::subscribe_tickets`, an injected clock over the real schema).
//!
//! Run it through `bash scripts/hub-test.sh subscribe_ticket`. Without `HUB_URL` every test
//! fails with a message saying so; nothing passes vacuously.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

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
             Run it with `bash scripts/hub-test.sh subscribe_ticket`."
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

/// One request with `Connection: close`; `token` adds `Authorization: Bearer <token>`.
fn http(method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> Reply {
    let mut stream = connect();
    let mut request = format!(
        "{method} {path} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        authority()
    );
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
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
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
    let status = head
        .split("\r\n")
        .next()
        .and_then(|line| line.split_whitespace().nth(1))
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line in {head:?}"));
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

/// A WebSocket upgrade request. `token` adds the Authorization header (the native way); the
/// browser way passes `None` and puts `ticket=` in the path. Returns the status line's code,
/// the response body or first frame text, and whether the socket was upgraded.
fn upgrade(path: &str, token: Option<&str>) -> (u16, String) {
    let mut stream = connect();
    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {}\r\n", authority());
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    request.push_str(
        "Upgrade: websocket\r\nConnection: Upgrade\r\n\
         Sec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\nSec-WebSocket-Version: 13\r\n\r\n",
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
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line in {head:?}"));
    if status != 101 {
        // Read the (short) JSON error body: chunked or sized, until the hub closes or goes quiet.
        stream
            .set_read_timeout(Some(Duration::from_secs(2)))
            .expect("set read timeout");
        let mut raw = Vec::new();
        let _ = stream.read_to_end(&mut raw);
        let raw = String::from_utf8_lossy(&raw).into_owned();
        let body = if head
            .to_ascii_lowercase()
            .contains("transfer-encoding: chunked")
        {
            dechunk(&raw)
        } else {
            raw
        };
        return (status, body);
    }
    // Upgraded: the hub speaks first (`hello`, then `snapshot`, ...).
    let mut chunk = [0u8; 4096];
    let n = stream.read(&mut chunk).expect("read the first frame");
    assert!(n > 2, "no frame after the upgrade");
    (status, String::from_utf8_lossy(&chunk[..n]).into_owned())
}

fn unique(name: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or_default();
    format!("th_{name}_{}_{nanos:x}", std::process::id())
}

/// A thread mapped to the dev org, as thread creation would.
fn hosted_thread(name: &str) -> String {
    let thread = unique(name);
    let hosted = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/host"),
        Some("dev"),
        Some(&json!({ "machine_id": "mch_subscribe_ticket" })),
    );
    assert_eq!(hosted.status, 200, "host thread: {}", hosted.body);
    thread
}

fn mint(thread: &str, token: &str) -> String {
    let reply = http(
        "POST",
        &format!("/v1/threads/{thread}/subscribe-ticket"),
        Some(token),
        None,
    );
    assert_eq!(reply.status, 201, "mint: {}", reply.body);
    let body = reply.json();
    assert!(body["expires_at"].as_str().is_some(), "{body}");
    let ticket = body["ticket"].as_str().expect("ticket").to_owned();
    assert!(
        ticket.starts_with("sst_") && ticket.len() == 68,
        "ticket shape: {ticket}"
    );
    ticket
}

fn viewer_session() -> (String, String) {
    let issued = http(
        "POST",
        "/v1/auth/sessions",
        Some("dev"),
        Some(&json!({ "role": "viewer", "display_name": "ticket viewer" })),
    );
    assert_eq!(issued.status, 201, "issue: {}", issued.body);
    let body = issued.json();
    (
        body["session"]["id"].as_str().expect("id").to_owned(),
        body["session"]["token"].as_str().expect("token").to_owned(),
    )
}

#[test]
fn a_ticket_upgrades_once_without_an_authorization_header() {
    let thread = hosted_thread("tkt_once");
    let ticket = mint(&thread, "dev");
    let path = format!("/v1/threads/{thread}/subscribe?after=0&ticket={ticket}");

    let (status, first_frame) = upgrade(&path, None);
    assert_eq!(status, 101, "first use: {first_frame}");
    assert!(first_frame.contains("hello"), "first frame: {first_frame}");

    // The second use of the same ticket is refused.
    let (status, body) = upgrade(&path, None);
    assert_eq!(status, 401, "reuse: {body}");
    let reused: Value = serde_json::from_str(&body).expect("error body");
    assert_eq!(reused["error"], "unauthorized");
}

#[test]
fn a_missing_garbage_or_other_thread_ticket_is_the_same_401() {
    let thread = hosted_thread("tkt_a");
    let other = hosted_thread("tkt_b");
    let ticket = mint(&thread, "dev");

    let none = upgrade(&format!("/v1/threads/{thread}/subscribe"), None);
    let garbage = upgrade(&format!("/v1/threads/{thread}/subscribe?ticket=nope"), None);
    let unknown = format!("sst_{}", "ab".repeat(32));
    let guessed = upgrade(
        &format!("/v1/threads/{thread}/subscribe?ticket={unknown}"),
        None,
    );
    // A ticket of another thread.
    let moved = upgrade(
        &format!("/v1/threads/{other}/subscribe?ticket={ticket}"),
        None,
    );
    for (status, body) in [&none, &garbage, &guessed, &moved] {
        assert_eq!(*status, 401, "{body}");
    }
    assert_eq!(none.1, garbage.1, "same body for every refusal");
    assert_eq!(none.1, guessed.1);
    assert_eq!(none.1, moved.1);
    assert!(!moved.1.contains(&ticket) && !garbage.1.contains("nope"));

    // The refused attempt on the wrong thread did not burn the ticket for its own thread.
    let (status, _) = upgrade(
        &format!("/v1/threads/{thread}/subscribe?ticket={ticket}"),
        None,
    );
    assert_eq!(status, 101);
}

#[test]
fn minting_needs_a_valid_session_and_a_thread_of_the_callers_org() {
    let thread = hosted_thread("tkt_auth");
    let path = format!("/v1/threads/{thread}/subscribe-ticket");
    assert_eq!(http("POST", &path, None, None).status, 401);
    assert_eq!(http("POST", &path, Some("smt_nope"), None).status, 401);
    // A thread without a host row is a 404, like subscribe.
    let missing = http(
        "POST",
        &format!("/v1/threads/{}/subscribe-ticket", unique("tkt_none")),
        Some("dev"),
        None,
    );
    assert_eq!(missing.status, 404, "{}", missing.body);
    // No GET: it is a POST route.
    assert_ne!(http("GET", &path, Some("dev"), None).status, 201);
}

#[test]
fn a_viewer_session_can_mint_and_a_revoked_session_loses_its_ticket() {
    let thread = hosted_thread("tkt_viewer");
    let (session_id, token) = viewer_session();

    // Viewer is enough, and the ticket opens the socket.
    let ticket = mint(&thread, &token);
    let (status, frame) = upgrade(
        &format!("/v1/threads/{thread}/subscribe?ticket={ticket}"),
        None,
    );
    assert_eq!(status, 101, "{frame}");

    // A ticket whose session is revoked before use is refused.
    let pending = mint(&thread, &token);
    let revoked = http(
        "DELETE",
        &format!("/v1/auth/sessions/{session_id}"),
        Some("dev"),
        None,
    );
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    let (status, body) = upgrade(
        &format!("/v1/threads/{thread}/subscribe?ticket={pending}"),
        None,
    );
    assert_eq!(status, 401, "{body}");
    // The revoked session cannot mint another.
    let again = http(
        "POST",
        &format!("/v1/threads/{thread}/subscribe-ticket"),
        Some(&token),
        None,
    );
    assert_eq!(again.status, 401);
}

#[test]
fn the_ticket_is_stripped_wherever_it_sits_in_the_query() {
    // The Worker strips `ticket=` before the Thread DO; the forwarded query must still parse
    // and the DO must still answer `hello`, for a ticket first, in the middle, last and repeated.
    let thread = hosted_thread("tkt_strip");
    let forms = [
        "ticket={t}&after=0",
        "after=0&ticket={t}",
        "after=0&ticket={t}&x=1",
        "ticket={t}",
    ];
    for form in forms {
        let ticket = mint(&thread, "dev");
        let query = form.replace("{t}", &ticket);
        let (status, frame) = upgrade(&format!("/v1/threads/{thread}/subscribe?{query}"), None);
        assert_eq!(status, 101, "{form}: {frame}");
        assert!(frame.contains("hello"), "{form}: first frame: {frame}");
    }
}

#[test]
fn the_authorization_header_subscribe_still_works_and_wins_over_a_ticket() {
    let thread = hosted_thread("tkt_header");
    let (status, frame) = upgrade(&format!("/v1/threads/{thread}/subscribe"), Some("dev"));
    assert_eq!(status, 101, "{frame}");
    // A bad Authorization header is not rescued by a ticket.
    let ticket = mint(&thread, "dev");
    let (status, _) = upgrade(
        &format!("/v1/threads/{thread}/subscribe?ticket={ticket}"),
        Some("smt_nope"),
    );
    assert_eq!(status, 401);
}
