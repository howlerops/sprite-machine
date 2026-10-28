//! M2-001 smoke test against a running hub (`wrangler dev --local`).
//!
//! Needs `HUB_URL` (for example `http://127.0.0.1:8787`). Without it every test fails with a
//! message saying so; nothing passes vacuously. Run it through `bash scripts/hub-test.sh smoke`,
//! which builds the Worker, starts wrangler and sets `HUB_URL`. The `live` feature (required by
//! `[[test]] smoke` in Cargo.toml) keeps this file out of a plain `cargo test -p hub`.
//!
//! The HTTP client is a few lines of `std::net` so the hub crate needs no HTTP dependency.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::time::Duration;

use serde_json::Value;

/// One HTTP/1.1 response, parsed just enough for these checks.
struct Reply {
    status: u16,
    headers: Vec<(String, String)>,
    body: String,
}

impl Reply {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
    }
}

/// `HUB_URL`, or a panic that says how to run the test.
fn hub_url() -> String {
    match std::env::var("HUB_URL") {
        Ok(url) if !url.trim().is_empty() => url.trim().trim_end_matches('/').to_owned(),
        _ => panic!(
            "HUB_URL is not set: this test needs a running hub. \
             Run it with `bash scripts/hub-test.sh smoke`."
        ),
    }
}

/// `GET path` against `HUB_URL` with `Connection: close`.
fn get(path: &str, extra_headers: &[(&str, &str)]) -> Reply {
    let url = hub_url();
    let authority = url
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"));
    let mut stream = TcpStream::connect(authority)
        .unwrap_or_else(|error| panic!("cannot connect to HUB_URL {url}: {error}"));
    stream
        .set_read_timeout(Some(Duration::from_secs(30)))
        .expect("set read timeout");

    let mut request = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
    for (name, value) in extra_headers {
        request.push_str(&format!("{name}: {value}\r\n"));
    }
    request.push_str("\r\n");
    stream.write_all(request.as_bytes()).expect("send request");

    let mut raw = Vec::new();
    stream.read_to_end(&mut raw).expect("read response");
    parse(&String::from_utf8_lossy(&raw))
}

fn parse(text: &str) -> Reply {
    let (head, body) = text
        .split_once("\r\n\r\n")
        .unwrap_or_else(|| panic!("malformed HTTP response: {text:?}"));
    let mut lines = head.split("\r\n");
    let status_line = lines.next().unwrap_or_default();
    let status = status_line
        .split_whitespace()
        .nth(1)
        .and_then(|code| code.parse::<u16>().ok())
        .unwrap_or_else(|| panic!("bad status line {status_line:?}"));
    let headers: Vec<(String, String)> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(name, value)| (name.trim().to_ascii_lowercase(), value.trim().to_owned()))
        .collect();
    let chunked = headers
        .iter()
        .any(|(name, value)| name == "transfer-encoding" && value.eq_ignore_ascii_case("chunked"));
    let body = if chunked {
        dechunk(body)
    } else {
        body.to_owned()
    };
    Reply {
        status,
        headers,
        body,
    }
}

/// Decodes a chunked body (`<hex size>\r\n<data>\r\n ... 0\r\n`).
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

#[test]
fn healthz_answers_200_with_json_build_info() {
    let reply = get("/healthz", &[]);
    assert_eq!(reply.status, 200, "GET /healthz body: {}", reply.body);
    assert!(
        reply
            .header("content-type")
            .is_some_and(|value| value.starts_with("application/json")),
        "GET /healthz is not JSON: {:?}",
        reply.header("content-type")
    );
    let json: Value = serde_json::from_str(&reply.body)
        .unwrap_or_else(|error| panic!("GET /healthz body is not JSON ({error}): {}", reply.body));
    assert_eq!(json["name"], "hub", "healthz body: {json}");
    assert!(
        json["version"].as_str().is_some_and(|v| !v.is_empty()),
        "healthz body has no version: {json}"
    );
}

#[test]
fn unknown_route_is_a_json_404_in_the_client_api_shape() {
    let reply = get("/no-such-route", &[]);
    assert_eq!(reply.status, 404, "body: {}", reply.body);
    let json: Value = serde_json::from_str(&reply.body)
        .unwrap_or_else(|error| panic!("404 body is not JSON ({error}): {}", reply.body));
    assert_eq!(json["error"], "not_found", "404 body: {json}");
    assert!(
        json["message"].is_string(),
        "404 body has no message: {json}"
    );
}

#[test]
fn responses_carry_x_request_id_generated_or_echoed() {
    let generated = get("/healthz", &[]);
    assert!(
        generated
            .header("x-request-id")
            .is_some_and(|id| !id.is_empty()),
        "x-request-id missing on a generated id: {:?}",
        generated.headers
    );

    let echoed = get("/healthz", &[("x-request-id", "smoke-test-42")]);
    assert_eq!(
        echoed.header("x-request-id"),
        Some("smoke-test-42"),
        "a valid caller x-request-id is echoed"
    );

    let not_found = get("/no-such-route", &[]);
    assert!(
        not_found.header("x-request-id").is_some(),
        "x-request-id missing on the 404"
    );
}
