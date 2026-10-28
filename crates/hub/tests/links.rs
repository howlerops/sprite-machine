//! M2-010 live tests of the deep links and the app-site association documents against a running
//! hub (`wrangler dev --local`): `GET /t/{id}` redirects to the web page's thread route, and the
//! two `.well-known` documents are served for the app that `scripts/hub-test.sh` configures with
//! `--var APPLE_*` and `ANDROID_*` (200 and the body).
//!
//! Needs `HUB_URL`. `bash scripts/hub-test.sh links` starts the hub and sets it. Without `HUB_URL`
//! every test fails with a message saying so; nothing passes vacuously. The `live` feature keeps
//! this file out of a plain `cargo test -p hub`. The document bodies for a configured app are
//! native tests (`src/links.rs`, which also covers the 404 of an empty config); this file checks
//! what the running Worker serves.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

const READ_TIMEOUT: Duration = Duration::from_secs(20);

struct Reply {
    status: u16,
    headers: String,
    body: String,
}

impl Reply {
    /// The value of a response header (case-insensitive name), if present.
    fn header(&self, name: &str) -> Option<String> {
        self.headers.lines().find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.trim()
                .eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }
}

fn authority() -> String {
    let url = match std::env::var("HUB_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_owned(),
        _ => panic!(
            "HUB_URL is not set: this test needs a running hub. \
             Run it with `bash scripts/hub-test.sh links`."
        ),
    };
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

/// One `GET` with `Connection: close`; redirects are not followed.
fn get(path: &str) -> Reply {
    let authority = authority();
    let mut stream = TcpStream::connect(&authority)
        .unwrap_or_else(|error| panic!("cannot connect to HUB_URL {authority}: {error}"));
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("set read timeout");
    let request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n\r\n");
    stream.write_all(request.as_bytes()).expect("send request");
    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    let text = String::from_utf8_lossy(&raw).into_owned();
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
    let status = head
        .split_whitespace()
        .nth(1)
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
    Reply {
        status,
        headers: head.to_owned(),
        body,
    }
}

fn dechunk(mut rest: &str) -> String {
    let mut out = String::new();
    while let Some((size_line, after)) = rest.split_once("\r\n") {
        let size = usize::from_str_radix(size_line.split(';').next().unwrap_or("").trim(), 16)
            .unwrap_or(0);
        if size == 0 || after.len() < size {
            break;
        }
        out.push_str(&after[..size]);
        rest = after[size..].strip_prefix("\r\n").unwrap_or("");
    }
    out
}

#[test]
fn a_thread_link_redirects_to_the_web_pages_thread_route() {
    let reply = get("/t/th_01JBX2");
    assert_eq!(reply.status, 302, "{}", reply.headers);
    assert_eq!(reply.header("location").as_deref(), Some("/#/t/th_01JBX2"));
}

#[test]
fn a_thread_link_with_a_bad_id_is_404_and_does_not_redirect() {
    let reply = get("/t/bad%20id");
    assert_eq!(reply.status, 404, "{}", reply.headers);
    assert_eq!(reply.header("location"), None);
}

/// The made-up app that `scripts/hub-test.sh` configures with `--var` (never a real app).
fn fingerprint() -> String {
    vec!["AB"; 32].join(":")
}

#[test]
fn the_association_documents_are_served_for_the_configured_app() {
    let apple = get("/.well-known/apple-app-site-association");
    assert_eq!(apple.status, 200, "{}", apple.body);
    assert!(
        apple
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{}",
        apple.headers
    );
    let document: serde_json::Value = serde_json::from_str(&apple.body).expect("AASA is JSON");
    assert_eq!(
        document["applinks"]["details"][0]["appID"],
        "ABCDE12345.dev.sprite.machine.test"
    );
    assert_eq!(document["applinks"]["details"][0]["paths"][0], "/t/*");

    let android = get("/.well-known/assetlinks.json");
    assert_eq!(android.status, 200, "{}", android.body);
    assert!(
        android
            .header("content-type")
            .is_some_and(|t| t.starts_with("application/json")),
        "{}",
        android.headers
    );
    let document: serde_json::Value = serde_json::from_str(&android.body).expect("links is JSON");
    assert_eq!(
        document[0]["target"]["package_name"],
        "dev.sprite.machine.test"
    );
    assert_eq!(
        document[0]["target"]["sha256_cert_fingerprints"][0],
        fingerprint().as_str()
    );
}
