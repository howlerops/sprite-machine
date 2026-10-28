//! Platform-neutral pieces of the hub HTTP surface (M2-001): the JSON bodies, request-id
//! handling and the structured log lines. The axum wiring and the Worker entries live in
//! `worker.rs` (wasm32 only). Everything here is plain Rust, so `cargo test -p hub` runs it
//! natively.

use std::sync::atomic::{AtomicU64, Ordering};

use serde_json::{Value, json};

use crate::auth::ticket;

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

/// The path and query a request is forwarded with, with its `ticket=` pairs removed (SPEC-006).
/// `None` when there is no ticket in the query, so nothing needs rewriting.
pub fn path_and_query_without_ticket(path: &str, query: Option<&str>) -> Option<String> {
    let query = query?;
    ticket::ticket_in_query(Some(query))?;
    let cleaned = ticket::strip_ticket(query);
    Some(if cleaned.is_empty() {
        path.to_owned()
    } else {
        format!("{path}?{cleaned}")
    })
}

/// One structured access-log line (JSON, one object per line) for the Workers console. Only the
/// path is logged: `path_and_query` is cut at its first `?`, so a query (a subscribe ticket, an
/// `after=` cursor) never reaches the logs.
pub fn access_log(
    request_id: &str,
    method: &str,
    path_and_query: &str,
    status: u16,
    duration_ms: u64,
) -> String {
    let path = path_and_query.split('?').next().unwrap_or_default();
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
    fn access_log_records_the_path_and_never_the_query() {
        let line = access_log(
            "req_2",
            "GET",
            "/v1/threads/th_1/subscribe?after=3&ticket=sst_abcdef0123",
            101,
            4,
        );
        let value: Value = serde_json::from_str(&line).expect("access log is JSON");
        assert_eq!(value["path"], "/v1/threads/th_1/subscribe");
        assert!(!line.contains("ticket"), "query key in log: {line}");
        assert!(!line.contains("sst_"), "ticket value in log: {line}");
        assert!(!line.contains("after"), "query key in log: {line}");
        assert!(!line.contains('?'), "query separator in log: {line}");
        // A ticket-only query and a bare `?` log the same path.
        for query_form in [
            "/v1/threads/th_1/subscribe?ticket=sst_x",
            "/v1/threads/th_1/subscribe?",
        ] {
            let line = access_log("req_3", "GET", query_form, 401, 1);
            let value: Value = serde_json::from_str(&line).expect("access log is JSON");
            assert_eq!(value["path"], "/v1/threads/th_1/subscribe");
            assert!(!line.contains("sst_"), "{line}");
        }
    }

    #[test]
    fn ticket_is_stripped_from_mixed_queries_before_forwarding() {
        let path = "/v1/threads/th_1/subscribe";
        let stripped = |query: &str| path_and_query_without_ticket(path, Some(query));
        // Ticket in the middle, first, last and alone; repeated keys are all removed.
        assert_eq!(
            stripped("after=3&ticket=sst_a&x=1").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=3&x=1")
        );
        assert_eq!(
            stripped("ticket=sst_a&after=3").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=3")
        );
        assert_eq!(
            stripped("after=3&ticket=sst_a").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=3")
        );
        assert_eq!(stripped("ticket=sst_a").as_deref(), Some(path));
        assert_eq!(
            stripped("ticket=sst_a&after=1&ticket=sst_b").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=1")
        );
        // An empty ticket is still a ticket pair (and is refused later).
        assert_eq!(
            stripped("ticket=&after=3").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=3")
        );
        // Keys that merely end in `ticket` are not the ticket and stay.
        assert_eq!(
            stripped("myticket=1&ticket=sst_a&after=3").as_deref(),
            Some("/v1/threads/th_1/subscribe?myticket=1&after=3")
        );
        // A percent-encoded value is still the ticket's value under a literal `ticket=` key.
        assert_eq!(
            stripped("after=1&ticket=sst%5Fabc").as_deref(),
            Some("/v1/threads/th_1/subscribe?after=1")
        );
        // No ticket, no rewrite.
        assert_eq!(path_and_query_without_ticket(path, Some("after=3")), None);
        assert_eq!(path_and_query_without_ticket(path, None), None);
        // Known gap, pinned: a percent-encoded key (`%74icket`) is not the ticket to either the
        // Worker or the consume step, so it is forwarded unchanged. See the report's discovered work.
        assert_eq!(
            path_and_query_without_ticket(path, Some("%74icket=sst_a&after=3")),
            None
        );
    }

    #[test]
    fn queue_log_names_the_queue() {
        let value: Value =
            serde_json::from_str(&queue_log("sm-notify", 2)).expect("queue log is JSON");
        assert_eq!(value["queue"], "sm-notify");
        assert_eq!(value["messages"], 2);
    }
}
