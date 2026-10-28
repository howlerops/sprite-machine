//! M2-003a live tests of hub auth against a running hub (`wrangler dev --local`): the one-time
//! owner bootstrap, session tokens (issue, use, expire, revoke), roles on the thread routes, and
//! the audit rows every mutation leaves.
//!
//! Needs `HUB_URL`, a hub started with `DEV_FIXTURES=1` and a `BOOTSTRAP_OWNER_TOKEN` var, and
//! the same token in this process's environment. `bash scripts/hub-test.sh authz` does all of
//! that with a random test token. Without them every test fails with a message saying so;
//! nothing passes vacuously. The `live` feature keeps this file out of a plain `cargo test -p
//! hub`. (The `DEV_FIXTURES`-off cases are native: `src/auth/tests.rs`.)
//!
//! The one successful bootstrap per hub is done by exactly one test; every other test gets its
//! sessions from the dev owner (`Bearer dev`) through `POST /v1/auth/sessions`.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

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
             Run it with `bash scripts/hub-test.sh authz`."
        ),
    };
    url.strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

fn bootstrap_secret() -> String {
    match std::env::var("BOOTSTRAP_OWNER_TOKEN") {
        Ok(token) if !token.trim().is_empty() => token,
        _ => panic!(
            "BOOTSTRAP_OWNER_TOKEN is not set: run this test with `bash scripts/hub-test.sh authz`."
        ),
    }
}

/// One request with `Connection: close`. `token` becomes `Authorization: Bearer <token>`.
fn http(
    method: &str,
    path: &str,
    token: Option<&str>,
    headers: &[(&str, &str)],
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
    if let Some(token) = token {
        request.push_str(&format!("Authorization: Bearer {token}\r\n"));
    }
    for (name, value) in headers {
        request.push_str(&format!("{name}: {value}\r\n"));
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

// ---- helpers ----------------------------------------------------------------------------

/// A name that is unique per call and per run.
fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!(
        "{prefix}_{nanos:x}_{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// Issues a session for a new user with `role` through the dev owner; returns
/// `(token, session id, user id)`.
fn issue(role: &str, ttl_seconds: Option<i64>) -> (String, String, String) {
    let mut body = json!({ "role": role, "display_name": unique("user") });
    if let Some(ttl) = ttl_seconds {
        body["ttl_seconds"] = json!(ttl);
    }
    let reply = http("POST", "/v1/auth/sessions", Some("dev"), &[], Some(&body));
    assert_eq!(reply.status, 201, "issue {role}: {}", reply.body);
    let out = reply.json();
    assert_eq!(out["role"], role);
    (
        out["session"]["token"].as_str().expect("token").to_owned(),
        out["session"]["id"].as_str().expect("id").to_owned(),
        out["user"]["id"].as_str().expect("user").to_owned(),
    )
}

/// A thread of the dev org, recorded through the init hook as thread creation does (thread
/// routes answer 404 for a thread without a host row).
fn hosted_thread() -> String {
    let thread = unique("thr");
    let reply = http(
        "POST",
        &format!("/v1/dev/threads/{thread}/init"),
        Some("dev"),
        &[],
        Some(&json!({
            "machine_id": unique("mch"),
            "agent": "claude",
            "title": "authz",
            "workspace": { "path": "/tmp/work" },
            "permission_mode": "default",
        })),
    );
    assert_eq!(reply.status, 200, "thread init: {}", reply.body);
    thread
}

fn prompt(thread: &str, token: &str, request_id: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/prompts"),
        Some(token),
        &[
            ("Idempotency-Key", &unique("idk")),
            ("x-request-id", request_id),
        ],
        Some(&json!({
            "task_id": unique("tk"),
            // A forged actor in the body must be ignored: the actor comes from the session.
            "actor": { "kind": "user", "id": "usr_forged" },
            "parts": [{ "type": "text", "text": "hello" }],
        })),
    )
}

fn approve(thread: &str, token: &str, request_id: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/interrupts/int_authz"),
        Some(token),
        &[
            ("Idempotency-Key", &unique("idk")),
            ("x-request-id", request_id),
        ],
        Some(&json!({ "outcome": { "type": "selected", "option_id": "allow" } })),
    )
}

fn cancel(thread: &str, token: &str, request_id: &str) -> Reply {
    http(
        "POST",
        &format!("/v1/threads/{thread}/cancel"),
        Some(token),
        &[
            ("Idempotency-Key", &unique("idk")),
            ("x-request-id", request_id),
        ],
        Some(&json!({})),
    )
}

fn delete_pending(thread: &str, token: &str, request_id: &str) -> Reply {
    http(
        "DELETE",
        &format!("/v1/threads/{thread}/pending/tk_authz"),
        Some(token),
        &[("x-request-id", request_id)],
        None,
    )
}

fn code(reply: &Reply) -> String {
    reply.json()["error"]
        .as_str()
        .unwrap_or_default()
        .to_owned()
}

/// The dev org's audit rows (the dev owner is an admin there).
fn audit_rows() -> Vec<Value> {
    let reply = http("GET", "/v1/dev/audit?limit=1000", Some("dev"), &[], None);
    assert_eq!(reply.status, 200, "audit read: {}", reply.body);
    reply.json()["entries"].as_array().expect("entries").clone()
}

fn rows_for<'a>(rows: &'a [Value], action: &str, request_id: &str) -> Vec<&'a Value> {
    rows.iter()
        .filter(|row| row["action"] == action && row["request_id"] == request_id)
        .collect()
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn bootstrap_creates_the_owner_once_and_refuses_a_second_attempt() {
    let secret = bootstrap_secret();
    let body = json!({ "org_name": "Authz Test Org", "user_name": "Root", "user_email": "root@example.com" });
    let first = http(
        "POST",
        "/v1/auth/bootstrap",
        Some(&secret),
        &[("x-request-id", "authz-bootstrap")],
        Some(&body),
    );
    assert_eq!(first.status, 201, "bootstrap: {}", first.body);
    let out = first.json();
    assert_eq!(out["role"], "owner");
    assert_eq!(out["org"]["name"], "Authz Test Org");
    let token = out["session"]["token"].as_str().expect("token").to_owned();
    assert!(
        token.starts_with("smt_") && token.len() >= 4 + 64,
        "token: {token}"
    );

    // The bootstrap session works: it is an owner, so it can issue sessions and read audit.
    let issued = http(
        "POST",
        "/v1/auth/sessions",
        Some(&token),
        &[],
        Some(&json!({ "role": "viewer" })),
    );
    assert_eq!(issued.status, 201, "owner issues: {}", issued.body);
    let audit = http("GET", "/v1/dev/audit", Some(&token), &[], None);
    assert_eq!(audit.status, 200);
    let rows = audit.json()["entries"].as_array().expect("entries").clone();
    let boot = rows_for(&rows, "auth.bootstrap", "authz-bootstrap");
    assert_eq!(boot.len(), 1, "audit rows: {rows:?}");
    assert_eq!(boot[0]["actor_user_id"], out["user"]["id"]);
    assert_eq!(boot[0]["target_kind"], "org");
    // Neither the session token nor the bootstrap secret is in any audit row.
    let dump = Value::Array(rows).to_string();
    assert!(
        !dump.contains(&token) && !dump.contains(&secret),
        "secret leaked into audit"
    );

    // A second attempt with the right token is refused.
    let second = http(
        "POST",
        "/v1/auth/bootstrap",
        Some(&secret),
        &[],
        Some(&body),
    );
    assert_eq!(second.status, 409, "second bootstrap: {}", second.body);
    assert_eq!(code(&second), "conflict");
}

#[test]
fn wrong_or_missing_bootstrap_token_is_401() {
    let _ = bootstrap_secret();
    let body = json!({ "org_name": "Nope", "user_name": "Mallory" });
    for token in [Some("wrong-token"), Some("dev"), Some(""), None] {
        let reply = http("POST", "/v1/auth/bootstrap", token, &[], Some(&body));
        assert_eq!(reply.status, 401, "token {token:?}: {}", reply.body);
        assert_eq!(code(&reply), "unauthorized");
    }
}

#[test]
fn session_issue_use_and_revoke() {
    let (token, session_id, _) = issue("member", None);
    let thread = hosted_thread();
    // Use: a member reads a thread of its org.
    let read = http(
        "GET",
        &format!("/v1/threads/{thread}"),
        Some(&token),
        &[],
        None,
    );
    assert_eq!(read.status, 200, "read: {}", read.body);

    let revoke = http(
        "DELETE",
        "/v1/auth/sessions/current",
        Some(&token),
        &[],
        None,
    );
    assert_eq!(revoke.status, 200, "revoke: {}", revoke.body);
    assert_eq!(revoke.json()["session_id"], session_id);
    let after = http(
        "GET",
        &format!("/v1/threads/{thread}"),
        Some(&token),
        &[],
        None,
    );
    assert_eq!(after.status, 401, "revoked token: {}", after.body);
    assert_eq!(code(&after), "unauthorized");

    // A member cannot issue sessions (admin only).
    let (member, _, _) = issue("member", None);
    let denied = http(
        "POST",
        "/v1/auth/sessions",
        Some(&member),
        &[],
        Some(&json!({})),
    );
    assert_eq!(denied.status, 403);
    assert_eq!(code(&denied), "forbidden");
}

#[test]
fn an_expired_session_is_401() {
    let (token, _, _) = issue("member", Some(1));
    let thread = hosted_thread();
    let path = format!("/v1/threads/{thread}");
    let before = http("GET", &path, Some(&token), &[], None);
    assert_eq!(before.status, 200, "fresh session works: {}", before.body);
    std::thread::sleep(Duration::from_millis(2500));
    let after = http("GET", &path, Some(&token), &[], None);
    assert_eq!(after.status, 401, "expired session: {}", after.body);
}

#[test]
fn an_admin_revokes_another_users_session_and_a_member_cannot() {
    let (victim, victim_session, _) = issue("member", None);
    let (member, _, _) = issue("member", None);
    let path = format!("/v1/auth/sessions/{victim_session}");
    let denied = http("DELETE", &path, Some(&member), &[], None);
    assert_eq!(denied.status, 403, "member revoke: {}", denied.body);
    let ok = http("DELETE", &path, Some("dev"), &[], None);
    assert_eq!(ok.status, 200, "admin revoke: {}", ok.body);
    let thread = hosted_thread();
    let after = http(
        "GET",
        &format!("/v1/threads/{thread}"),
        Some(&victim),
        &[],
        None,
    );
    assert_eq!(after.status, 401);
    let unknown = http(
        "DELETE",
        "/v1/auth/sessions/ses_nope",
        Some("dev"),
        &[],
        None,
    );
    assert_eq!(unknown.status, 404);
}

#[test]
fn unknown_and_malformed_tokens_are_401() {
    let thread = hosted_thread();
    let path = format!("/v1/threads/{thread}");
    let forged = format!("smt_{}", "0".repeat(64));
    for token in [None, Some("garbage"), Some(forged.as_str()), Some("smt_")] {
        let reply = http("GET", &path, token, &[], None);
        assert_eq!(reply.status, 401, "token {token:?}: {}", reply.body);
        assert_eq!(code(&reply), "unauthorized");
    }
}

#[test]
fn a_viewer_cannot_send_prompts_approve_cancel_or_delete() {
    let (viewer, _, _) = issue("viewer", None);
    let thread = hosted_thread();
    let request = unique("authz-viewer");
    for (name, reply) in [
        ("prompt", prompt(&thread, &viewer, &request)),
        ("approve", approve(&thread, &viewer, &request)),
        ("cancel", cancel(&thread, &viewer, &request)),
        ("delete", delete_pending(&thread, &viewer, &request)),
        (
            "create thread",
            http(
                "POST",
                "/v1/threads",
                Some(&viewer),
                &[
                    ("Idempotency-Key", &unique("idk")),
                    ("x-request-id", &request),
                ],
                Some(&json!({ "machine_id": unique("mch"), "agent": "claude" })),
            ),
        ),
    ] {
        assert_eq!(reply.status, 403, "viewer {name}: {}", reply.body);
        assert_eq!(code(&reply), "forbidden", "viewer {name}");
    }
    // Reads are fine for a viewer.
    let read = http(
        "GET",
        &format!("/v1/threads/{thread}/pending"),
        Some(&viewer),
        &[],
        None,
    );
    assert_eq!(read.status, 200, "viewer read: {}", read.body);
    // Refused requests leave no audit row (their request id appears nowhere in the log).
    let leaked: Vec<Value> = audit_rows()
        .into_iter()
        .filter(|row| row["request_id"] == request.as_str())
        .collect();
    assert!(
        leaked.is_empty(),
        "refused requests were audited: {leaked:?}"
    );
}

#[test]
fn a_member_can_send_approve_and_cancel_but_not_delete() {
    let (member, _, user_id) = issue("member", None);
    let thread = hosted_thread();
    let sent = prompt(&thread, &member, "authz-member");
    assert!(
        sent.status < 300,
        "member prompt: {} {}",
        sent.status,
        sent.body
    );
    let approved = approve(&thread, &member, "authz-member");
    assert_ne!(approved.status, 403, "member approve: {}", approved.body);
    assert_ne!(approved.status, 401);
    let cancelled = cancel(&thread, &member, "authz-member");
    assert_ne!(cancelled.status, 403, "member cancel: {}", cancelled.body);
    assert_ne!(cancelled.status, 401);
    let deleted = delete_pending(&thread, &member, "authz-member");
    assert_eq!(deleted.status, 403, "member delete: {}", deleted.body);

    // The actor is the session's user, not the forged one in the prompt body.
    let rows = audit_rows();
    let sent_rows = rows_for(&rows, "thread.prompt", "authz-member");
    let mine: Vec<_> = sent_rows
        .iter()
        .filter(|row| row["target_id"] == thread.as_str())
        .collect();
    assert_eq!(mine.len(), 1, "audit: {rows:?}");
    assert_eq!(mine[0]["actor_user_id"], user_id);
}

#[test]
fn an_admin_may_delete_pending_and_every_mutation_is_audited() {
    let (admin, _, admin_user) = issue("admin", None);
    let (member, member_session, member_user) = issue("member", None);
    let thread = hosted_thread();
    let request = unique("authz-audit");

    prompt(&thread, &member, &request);
    approve(&thread, &member, &request);
    cancel(&thread, &member, &request);
    let deleted = delete_pending(&thread, &admin, &request);
    assert_ne!(deleted.status, 403, "admin delete: {}", deleted.body);
    assert_ne!(deleted.status, 401);
    let revoked = http(
        "DELETE",
        &format!("/v1/auth/sessions/{member_session}"),
        Some(&admin),
        &[("x-request-id", &request)],
        None,
    );
    assert_eq!(revoked.status, 200);
    let issued = http(
        "POST",
        "/v1/auth/sessions",
        Some(&admin),
        &[("x-request-id", &request)],
        Some(&json!({ "role": "viewer" })),
    );
    assert_eq!(issued.status, 201);
    let issued_token = issued.json()["session"]["token"]
        .as_str()
        .expect("token")
        .to_owned();

    let rows = audit_rows();
    let expect = [
        ("thread.prompt", &member_user, "thread", thread.as_str()),
        (
            "thread.approval.resolve",
            &member_user,
            "thread",
            thread.as_str(),
        ),
        ("thread.cancel", &member_user, "thread", thread.as_str()),
        (
            "thread.pending.delete",
            &admin_user,
            "thread",
            thread.as_str(),
        ),
        (
            "auth.session.revoke",
            &admin_user,
            "session",
            member_session.as_str(),
        ),
    ];
    for (action, actor, kind, target) in expect {
        let found = rows_for(&rows, action, &request);
        assert_eq!(found.len(), 1, "{action}: audit rows {rows:?}");
        assert_eq!(found[0]["actor_user_id"], actor.as_str(), "{action}");
        assert_eq!(found[0]["target_kind"], kind, "{action}");
        assert_eq!(found[0]["target_id"], target, "{action}");
        assert!(found[0]["actor_device_id"].is_string(), "{action}");
    }
    let issue_rows = rows_for(&rows, "auth.session.issue", &request);
    assert_eq!(issue_rows.len(), 1);
    assert_eq!(issue_rows[0]["detail"]["role"], "viewer");
    // Detail never carries a token.
    let dump = Value::Array(rows.clone()).to_string();
    for secret in [&admin, &member, &issued_token] {
        assert!(!dump.contains(secret.as_str()), "token leaked into audit");
    }
}

#[test]
fn the_dev_audit_read_needs_an_admin() {
    let (member, _, _) = issue("member", None);
    let denied = http("GET", "/v1/dev/audit", Some(&member), &[], None);
    assert_eq!(denied.status, 403);
    let anonymous = http("GET", "/v1/dev/audit", None, &[], None);
    assert_eq!(anonymous.status, 401);
}
