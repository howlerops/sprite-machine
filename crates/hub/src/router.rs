//! Platform-neutral pieces of the hub HTTP surface (M2-001): the JSON bodies, request-id
//! handling and the structured log lines. The axum wiring and the Worker entries live in
//! `worker.rs` (wasm32 only). Everything here is plain Rust, so `cargo test -p hub` runs it
//! natively.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

/// Request id header, echoed when the caller sends a valid one and generated otherwise.
pub const REQUEST_ID_HEADER: &str = "x-request-id";

/// Longest request id we echo back; longer ones are replaced by a generated id.
const MAX_REQUEST_ID_LEN: usize = 128;

/// Per-isolate counter, so two ids minted in the same millisecond still differ.
static REQUEST_SEQ: AtomicU64 = AtomicU64::new(0);

/// Body of `GET /healthz`: the build info of this Worker.
pub fn health_body() -> Value {
    json!({
        "name": env!("CARGO_PKG_NAME"),
        "version": env!("CARGO_PKG_VERSION"),
    })
}

/// The error body of client-api.md §4 (`error` is a stable code, `message` is for humans).
pub fn api_error(error: &str, message: &str) -> Value {
    json!({ "error": error, "message": message })
}

/// A caller-supplied request id is echoed only when it is short and made of token characters
/// (letters, digits, `-`, `_`, `.`), so it cannot inject anything into logs or headers.
pub fn valid_request_id(candidate: &str) -> bool {
    !candidate.is_empty()
        && candidate.len() <= MAX_REQUEST_ID_LEN
        && candidate
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.'))
}

/// The request id for one request: the caller's if valid, otherwise a fresh one.
pub fn request_id(incoming: Option<&str>, now_ms: u64) -> String {
    match incoming {
        Some(id) if valid_request_id(id) => id.to_owned(),
        _ => new_request_id(now_ms),
    }
}

/// A fresh request id: the clock in milliseconds plus a per-isolate sequence number.
pub fn new_request_id(now_ms: u64) -> String {
    let seq = REQUEST_SEQ.fetch_add(1, Ordering::Relaxed);
    format!("req_{now_ms:x}_{seq:x}")
}

/// One structured access-log line (JSON, one object per line) for the Workers console.
pub fn access_log(
    request_id: &str,
    method: &str,
    path: &str,
    status: u16,
    duration_ms: u64,
) -> String {
    json!({
        "level": "info",
        "msg": "request",
        "request_id": request_id,
        "method": method,
        "path": path,
        "status": status,
        "duration_ms": duration_ms,
    })
    .to_string()
}

/// One structured log line for a consumed queue batch.
pub fn queue_log(queue: &str, messages: usize) -> String {
    json!({
        "level": "info",
        "msg": "queue batch acknowledged",
        "queue": queue,
        "messages": messages,
    })
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_body_has_build_info() {
        let body = health_body();
        assert_eq!(body["name"], "hub");
        assert_eq!(body["version"], env!("CARGO_PKG_VERSION"));
    }

    #[test]
    fn api_error_has_the_client_api_shape() {
        let body = api_error("not_found", "no route for GET /nope");
        assert_eq!(body["error"], "not_found");
        assert_eq!(body["message"], "no route for GET /nope");
        assert_eq!(body.as_object().map(|o| o.len()), Some(2));
    }

    #[test]
    fn valid_request_ids_are_token_characters_only() {
        assert!(valid_request_id("abc-123_x.y"));
        assert!(!valid_request_id(""));
        assert!(!valid_request_id("has space"));
        assert!(!valid_request_id("line\nbreak"));
        assert!(!valid_request_id("semi;colon"));
        assert!(valid_request_id(&"a".repeat(MAX_REQUEST_ID_LEN)));
        assert!(!valid_request_id(&"a".repeat(MAX_REQUEST_ID_LEN + 1)));
    }

    #[test]
    fn request_id_echoes_valid_and_replaces_invalid() {
        assert_eq!(request_id(Some("trace-7"), 1), "trace-7");
        let generated = request_id(Some("bad id"), 1);
        assert!(generated.starts_with("req_"));
        assert!(valid_request_id(&generated));
        assert!(request_id(None, 1).starts_with("req_"));
    }

    #[test]
    fn generated_request_ids_are_unique() {
        let a = new_request_id(5);
        let b = new_request_id(5);
        assert_ne!(a, b);
    }

    #[test]
    fn access_log_is_one_json_object() {
        let line = access_log("req_1", "GET", "/healthz", 200, 3);
        let value: Value = serde_json::from_str(&line).expect("access log is JSON");
        assert_eq!(value["msg"], "request");
        assert_eq!(value["request_id"], "req_1");
        assert_eq!(value["method"], "GET");
        assert_eq!(value["path"], "/healthz");
        assert_eq!(value["status"], 200);
        assert_eq!(value["duration_ms"], 3);
    }

    #[test]
    fn queue_log_names_the_queue() {
        let value: Value =
            serde_json::from_str(&queue_log("sm-notify", 2)).expect("queue log is JSON");
        assert_eq!(value["queue"], "sm-notify");
        assert_eq!(value["messages"], 2);
    }
}
