//! M2-010 live tests of the deep links and the app-site association documents against a running
//! hub (`wrangler dev --local`): `GET /t/{id}` redirects to the web page's thread route, and the
//! two `.well-known` documents are `404` while their config vars are empty (the default).
//!
//! Needs `HUB_URL`. `bash scripts/hub-test.sh links` starts the hub and sets it. Without `HUB_URL`
//! every test fails with a message saying so; nothing passes vacuously. The `live` feature keeps
//! this file out of a plain `cargo test -p hub`. The document bodies for a configured app are
//! native tests (`src/links.rs`); this file checks what the running Worker serves.

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
    Reply {
        status,
        headers: head.to_owned(),
        body: body.to_owned(),
    }
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

#[test]
fn the_association_documents_are_404_while_unconfigured() {
    // The default `[vars]` are empty: no app is claimed, so no document is served.
    for path in [
        "/.well-known/apple-app-site-association",
        "/.well-known/assetlinks.json",
    ] {
        let reply = get(path);
        assert_eq!(reply.status, 404, "{path}: {}", reply.body);
        assert_eq!(
            reply
                .header("content-type")
                .map(|t| t.starts_with("application/json")),
            Some(true)
        );
    }
}
