//! M2-009 live tests of the attachment routes against a running hub (`wrangler dev --local`).
//!
//! Needs `HUB_URL` (for example `http://127.0.0.1:8787`) and a hub started with
//! `DEV_FIXTURES=1`, which `crates/hub/.dev.vars` sets for `wrangler dev`. Run it through
//! `bash scripts/hub-test.sh attachments`, which builds the Worker, starts wrangler and sets
//! `HUB_URL`. Without `HUB_URL` every test fails with a message saying so; nothing passes
//! vacuously. The `live` feature (required by `[[test]] attachments`) keeps this file out of a
//! plain `cargo test -p hub`.
//!
//! The bodies are binary, so the HTTP client here works on bytes. Each test uses its own
//! idempotency keys, so reruns against the same hub do not collide.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

/// The attachment limit (client-api.md §5.6), in bytes.
const MAX_BYTES: usize = 20 * 1024 * 1024;

// ---- plain HTTP over bytes --------------------------------------------------------------

struct Reply {
    status: u16,
    head: String,
    body: Vec<u8>,
}

impl Reply {
    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap_or_else(|error| {
            panic!(
                "body is not JSON ({error}): {:?}",
                String::from_utf8_lossy(&self.body)
            )
        })
    }

    fn header(&self, name: &str) -> Option<String> {
        self.head.lines().skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

    fn error_code(&self) -> String {
        self.json()["error"].as_str().unwrap_or_default().to_owned()
    }
}

/// The hub's `http://host:port` from `HUB_URL`.
fn base() -> String {
    match std::env::var("HUB_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_owned(),
        _ => panic!(
            "HUB_URL is not set: this test needs a running hub. \
             Run it with `bash scripts/hub-test.sh attachments`."
        ),
    }
}

fn authority() -> String {
    base()
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port"))
        .to_owned()
}

/// The path and query of a URL the hub returned. The URL must start with the hub's own origin.
fn target_of(url: &str) -> String {
    assert!(
        url.starts_with(&base()),
        "a signed URL must start with the hub's origin {}: {url}",
        base()
    );
    let rest = &url[base().len()..];
    rest.to_owned()
}

fn send(
    method: &str,
    target: &str,
    auth: bool,
    idempotency_key: Option<&str>,
    body: &[u8],
) -> Reply {
    let mut stream = TcpStream::connect(authority()).expect("connect to HUB_URL");
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    let mut head = format!(
        "{method} {target} HTTP/1.1\r\nHost: {}\r\nConnection: close\r\n",
        authority()
    );
    if auth {
        head.push_str("Authorization: Bearer dev\r\n");
    }
    if let Some(key) = idempotency_key {
        head.push_str(&format!("Idempotency-Key: {key}\r\n"));
    }
    if !body.is_empty() {
        head.push_str("Content-Type: application/octet-stream\r\n");
    }
    head.push_str(&format!("Content-Length: {}\r\n\r\n", body.len()));
    stream.write_all(head.as_bytes()).expect("send head");
    // The hub may answer and close before it has read a large body (413). The reply is what
    // counts, so a failed write is not an error here.
    let _ = stream.write_all(body);

    let mut raw = Vec::new();
    let _ = stream.read_to_end(&mut raw);
    parse_reply(&raw)
}

fn find(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    haystack.windows(needle.len()).position(|w| w == needle)
}

fn parse_reply(raw: &[u8]) -> Reply {
    let split = find(raw, b"\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response ({} bytes)", raw.len()));
    let head = String::from_utf8_lossy(&raw[..split]).into_owned();
    let rest = &raw[split + 4..];
    let status = head
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line in {head:?}"));
    let chunked = head
        .to_ascii_lowercase()
        .contains("transfer-encoding: chunked");
    let body = if chunked {
        dechunk(rest)
    } else {
        rest.to_vec()
    };
    Reply { status, head, body }
}

fn dechunk(mut rest: &[u8]) -> Vec<u8> {
    let mut out = Vec::new();
    while let Some(line_end) = find(rest, b"\r\n") {
        let size_line = String::from_utf8_lossy(&rest[..line_end]).into_owned();
        let size_hex = size_line.split(';').next().unwrap_or_default().trim();
        let Ok(size) = usize::from_str_radix(size_hex, 16) else {
            break;
        };
        if size == 0 {
            break;
        }
        let start = line_end + 2;
        let Some(data) = rest.get(start..start + size) else {
            break;
        };
        out.extend_from_slice(data);
        rest = rest.get(start + size + 2..).unwrap_or_default();
    }
    out
}

// ---- helpers ----------------------------------------------------------------------------

/// A key no earlier run used: the clock in nanoseconds plus a tag.
fn unique(tag: &str) -> String {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock after 1970")
        .as_nanos();
    format!("idk_{nanos}_{tag}")
}

fn create_body(name: &str, mime: &str, size: usize) -> Value {
    json!({ "name": name, "mime_type": mime, "size": size })
}

fn create(key: &str, body: &Value) -> Reply {
    send(
        "POST",
        "/v1/attachments",
        true,
        Some(key),
        body.to_string().as_bytes(),
    )
}

/// Creates an attachment of `size` bytes and returns its answer, which must be a 201.
fn created(tag: &str, mime: &str, size: usize) -> Value {
    let reply = create(&unique(tag), &create_body("file.bin", mime, size));
    assert_eq!(reply.status, 201, "create answers 201");
    reply.json()
}

fn text_of(answer: &Value, key: &str) -> String {
    answer[key]
        .as_str()
        .unwrap_or_else(|| panic!("{key} is a string"))
        .to_owned()
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn create_needs_an_idempotency_key() {
    let reply = send(
        "POST",
        "/v1/attachments",
        true,
        None,
        create_body("a.png", "image/png", 4).to_string().as_bytes(),
    );
    assert_eq!(reply.status, 400);
    assert_eq!(reply.error_code(), "idempotency_key_required");
}

#[test]
fn create_upload_and_fetch_round_trip_the_bytes_with_the_declared_type() {
    let bytes: Vec<u8> = (0..1024).map(|i| (i % 251) as u8).collect();
    let key = unique("roundtrip");
    let reply = create(&key, &create_body("photo.png", "image/png", bytes.len()));
    assert_eq!(reply.status, 201, "create answers 201");
    let answer = reply.json();

    let upload = text_of(&answer, "upload_url");
    let url = text_of(&answer, "url");
    assert!(
        upload.contains("/upload?"),
        "the upload URL is the PUT target"
    );
    assert!(answer["expires_at"].is_string());
    assert_eq!(
        answer["part"],
        json!({
            "type": "file",
            "url": url,
            "name": "photo.png",
            "mime_type": "image/png",
            "size": bytes.len(),
        }),
        "the answer carries the FilePart a prompt uses"
    );

    let before = send("GET", &target_of(&url), false, None, &[]);
    assert_eq!(before.status, 404, "nothing is served before the upload");

    let put = send("PUT", &target_of(&upload), false, None, &bytes);
    assert_eq!(put.status, 204, "the PUT stores the bytes");

    let got = send("GET", &target_of(&url), false, None, &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.header("content-type").as_deref(), Some("image/png"));
    assert_eq!(
        got.header("x-content-type-options").as_deref(),
        Some("nosniff")
    );
    assert_eq!(got.body, bytes, "the bytes come back unchanged");
}

#[test]
fn the_same_key_replays_and_another_body_under_it_is_422() {
    let key = unique("replay");
    let body = create_body("notes.txt", "text/plain", 12);
    let first = create(&key, &body);
    let second = create(&key, &body);
    assert_eq!(first.status, 201);
    assert_eq!(second.status, 201);
    assert_eq!(
        first.json(),
        second.json(),
        "the same key and body replays the stored answer"
    );

    let clash = create(&key, &create_body("notes.txt", "text/plain", 13));
    assert_eq!(clash.status, 422);
    assert_eq!(clash.error_code(), "idempotency_mismatch");
}

#[test]
fn an_upload_of_the_wrong_size_or_over_twenty_megabytes_is_refused() {
    let short = created("short", "application/octet-stream", 16);
    let wrong = send(
        "PUT",
        &target_of(&text_of(&short, "upload_url")),
        false,
        None,
        &[1u8; 17],
    );
    assert_eq!(wrong.status, 400, "a body of another size than declared");
    let still_missing = send("GET", &target_of(&text_of(&short, "url")), false, None, &[]);
    assert_eq!(still_missing.status, 404, "a refused upload stores nothing");

    let big = created("big", "application/octet-stream", MAX_BYTES);
    let too_big = send(
        "PUT",
        &target_of(&text_of(&big, "upload_url")),
        false,
        None,
        &vec![0u8; MAX_BYTES + 1],
    );
    assert_eq!(too_big.status, 413, "over 20 MB is refused");
    let nothing = send("GET", &target_of(&text_of(&big, "url")), false, None, &[]);
    assert_eq!(nothing.status, 404);
}

#[test]
fn tampered_and_cross_object_urls_are_forbidden() {
    let mine = created("mine", "text/plain", 8);
    let other = created("other", "text/plain", 8);
    let bytes = b"abcdefgh".to_vec();
    let put = send(
        "PUT",
        &target_of(&text_of(&mine, "upload_url")),
        false,
        None,
        &bytes,
    );
    assert_eq!(put.status, 204);
    let put_other = send(
        "PUT",
        &target_of(&text_of(&other, "upload_url")),
        false,
        None,
        &bytes,
    );
    assert_eq!(put_other.status, 204);

    let url = text_of(&mine, "url");
    let target = target_of(&url);
    let (path, query) = target.split_once('?').expect("the content URL has a query");
    let signature = query.rsplit("sig=").next().expect("a signature");

    // A flipped signature character.
    let flipped_first = if signature.starts_with('0') { '1' } else { '0' };
    let forged_signature = format!("{flipped_first}{}", &signature[1..]);
    let forged = format!("{path}?{}", query.replace(signature, &forged_signature));
    assert_eq!(
        send("GET", &forged, false, None, &[]).status,
        403,
        "a forged signature"
    );

    // A changed expiry, under the old signature.
    let later = query.replacen("exp=", "exp=9", 1);
    assert_eq!(
        send("GET", &format!("{path}?{later}"), false, None, &[]).status,
        403
    );

    // Another attachment's path with this attachment's signature.
    let other_path = target_of(&text_of(&other, "url"));
    let other_path = other_path.split_once('?').expect("query").0;
    assert_eq!(
        send("GET", &format!("{other_path}?{query}"), false, None, &[]).status,
        403,
        "a signature does not move to another attachment"
    );

    // Another org's path with this attachment's signature.
    let other_org = path.replacen("/v1/attachments/", "/v1/attachments/org_other_", 1);
    assert_eq!(
        send("GET", &format!("{other_org}?{query}"), false, None, &[]).status,
        403,
        "a signature does not move to another org"
    );

    // The real URL still works.
    let got = send("GET", &target, false, None, &[]);
    assert_eq!(got.status, 200);
    assert_eq!(got.body, bytes);
}

#[test]
fn an_upload_url_cannot_read_and_a_content_url_cannot_write() {
    let answer = created("ops", "text/plain", 4);
    let upload_target = target_of(&text_of(&answer, "upload_url"));
    let content_target = target_of(&text_of(&answer, "url"));

    let read_with_put = send(
        "GET",
        &format!(
            "{}?{}",
            content_target.split_once('?').expect("query").0,
            upload_target.split_once('?').expect("query").1
        ),
        false,
        None,
        &[],
    );
    assert_eq!(read_with_put.status, 403, "a PUT signature does not read");

    let write_with_get = send(
        "PUT",
        &format!(
            "{}/upload?{}",
            content_target.split_once('?').expect("query").0,
            content_target.split_once('?').expect("query").1
        ),
        false,
        None,
        b"abcd",
    );
    assert_eq!(write_with_get.status, 403, "a GET signature does not write");
}
