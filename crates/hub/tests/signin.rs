//! M2-003c live tests of browser sign-in (client-api.md §5.7) against a running hub
//! (`wrangler dev --local`): a link minted by a paired machine and redeemed once into a capped
//! session, and the browser device-code flow (RFC 8628 errors, single use, rate limits).
//!
//! Needs `HUB_URL`, a hub with a `BOOTSTRAP_OWNER_TOKEN` var and the same token in this
//! process's environment. `bash scripts/hub-test.sh signin` does all of that. Without them every
//! test fails with a message saying so; nothing passes vacuously. The `live` feature keeps this
//! file out of a plain `cargo test -p hub`. The 5-minute link expiry and the 10-minute pairing
//! expiry need a clock the live hub cannot move: they are native tests in
//! `src/devices/tests/signin_tests.rs`.
//!
//! The one bootstrap per hub is done once, lazily, and its owner session approves the devices.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde_json::{Value, json};

const READ_TIMEOUT: Duration = Duration::from_secs(20);
/// The first poll interval the hub hands out (RFC 8628 default).
const INTERVAL: Duration = Duration::from_millis(5_300);

// ---- plain HTTP -------------------------------------------------------------------------

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
    /// A response header's value (name in any case).
    fn header(&self, name: &str) -> Option<String> {
        self.head.split("\r\n").skip(1).find_map(|line| {
            let (key, value) = line.split_once(':')?;
            key.eq_ignore_ascii_case(name)
                .then(|| value.trim().to_owned())
        })
    }

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
             Run it with `bash scripts/hub-test.sh signin`."
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
            "BOOTSTRAP_OWNER_TOKEN is not set: run this test with `bash scripts/hub-test.sh signin`."
        ),
    }
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
    Reply {
        status,
        head: head.to_owned(),
        body,
    }
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

fn unique(prefix: &str) -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_nanos();
    format!(
        "{prefix}-{nanos:x}-{}",
        COUNTER.fetch_add(1, Ordering::Relaxed)
    )
}

/// The bootstrap owner's session token (the hub allows one bootstrap, so it is shared).
fn owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        let reply = http(
            "POST",
            "/v1/auth/bootstrap",
            Some(&bootstrap_secret()),
            Some(&json!({ "org_name": "Signin Test", "user_name": "Owner" })),
        );
        assert_eq!(reply.status, 201, "bootstrap: {}", reply.body);
        reply.json()["session"]["token"]
            .as_str()
            .expect("owner token")
            .to_owned()
    })
}

/// A session with `role` in the owner's org.
fn session(role: &str) -> String {
    let reply = http(
        "POST",
        "/v1/auth/sessions",
        Some(owner()),
        Some(&json!({ "role": role, "display_name": unique("user") })),
    );
    assert_eq!(reply.status, 201, "issue {role}: {}", reply.body);
    reply.json()["session"]["token"]
        .as_str()
        .expect("token")
        .to_owned()
}

struct Pairing {
    device_code: String,
    user_code: String,
}

fn start() -> Pairing {
    let reply = http("POST", "/v1/devices/code", None, Some(&json!({})));
    assert_eq!(reply.status, 200, "code: {}", reply.body);
    let body = reply.json();
    Pairing {
        device_code: body["device_code"].as_str().expect("device_code").into(),
        user_code: body["user_code"].as_str().expect("user_code").into(),
    }
}

fn poll(device_code: &str) -> Reply {
    http(
        "POST",
        "/v1/devices/token",
        None,
        Some(&json!({ "device_code": device_code })),
    )
}

/// Failed user-code attempts are limited per org (5 a minute); the one test that exhausts the
/// limit takes the write lock, every approve takes the read lock, so they never overlap.
static APPROVE_LIMIT: RwLock<()> = RwLock::new(());

fn approve(token: Option<&str>, user_code: &str, name: &str) -> Reply {
    let _shared = APPROVE_LIMIT.read().unwrap_or_else(|e| e.into_inner());
    http(
        "POST",
        "/v1/devices/approve",
        token,
        Some(&json!({ "user_code": user_code, "name": name })),
    )
}

/// A machine paired through the device flow (approved by the owner): `(machine_id, credential)`.
fn paired_machine() -> (String, String) {
    let pairing = start();
    let approved = approve(Some(owner()), &pairing.user_code, &unique("laptop"));
    assert_eq!(approved.status, 200, "{}", approved.body);
    let granted = poll(&pairing.device_code);
    assert_eq!(granted.status, 200, "{}", granted.body);
    let granted = granted.json();
    (
        granted["machine_id"].as_str().expect("machine id").into(),
        granted["credential"].as_str().expect("credential").into(),
    )
}

fn mint(credential: &str) -> Reply {
    http(
        "POST",
        "/v1/devices/signin-links",
        Some(credential),
        Some(&json!({})),
    )
}

fn redeem(code: &str) -> Reply {
    http(
        "POST",
        "/v1/auth/links/redeem",
        None,
        Some(&json!({ "code": code })),
    )
}

fn code_of(url: &str) -> String {
    url.split_once("/pair?code=")
        .unwrap_or_else(|| panic!("not a pair url: {url}"))
        .1
        .to_owned()
}

fn threads_status(token: &str) -> u16 {
    http("GET", "/v1/threads", Some(token), None).status
}

// ---- A: the link a paired machine mints ---------------------------------------------------

#[test]
fn a_paired_machine_mints_a_link_that_redeems_exactly_once() {
    let (_, credential) = paired_machine();
    let minted = mint(&credential);
    assert_eq!(minted.status, 201, "{}", minted.body);
    let minted = minted.json();
    let url = minted["url"].as_str().expect("url");
    // Scheme and host come from the request.
    let expected = format!("http://{}/pair?code=sli_", authority());
    assert!(
        url.starts_with(&expected),
        "{url} should start with {expected}"
    );
    let code = code_of(url);
    assert_eq!(code.len(), 4 + 64, "256 random bits, hex");
    let expires_at = minted["expires_at"].as_str().expect("expires_at");
    assert!(
        expires_at.ends_with('Z') && expires_at.contains('T'),
        "{expires_at}"
    );

    // No auth on redeem; the session is the approver's, in their org, capped at member.
    let redeemed = redeem(&code);
    assert_eq!(redeemed.status, 201, "{}", redeemed.body);
    let session = redeemed.json();
    let token = session["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("smt_"));
    assert_eq!(session["user"]["display_name"], "Owner");
    assert_eq!(session["org"]["name"], "Signin Test");
    assert_eq!(session["role"], "member");
    assert!(session["expires_at"].is_string());
    assert_eq!(threads_status(&token), 200, "the session works");
    // Capped: the owner may issue sessions, this session may not.
    let issue = http(
        "POST",
        "/v1/auth/sessions",
        Some(&token),
        Some(&json!({ "role": "member" })),
    );
    assert_eq!(issue.status, 403, "{}", issue.body);

    // Single use, and every refusal is the same 410 `expired`.
    let unknown = format!("sli_{}", "0".repeat(64));
    for code in [code.as_str(), unknown.as_str(), "not-a-code"] {
        let again = redeem(code);
        assert_eq!(again.status, 410, "{}", again.body);
        assert_eq!(again.json()["error"], "expired");
    }
    let missing = http("POST", "/v1/auth/links/redeem", None, Some(&json!({})));
    assert_eq!(missing.status, 400, "{}", missing.body);
}

#[test]
fn only_a_device_origin_live_machine_mints_links() {
    // An admin-API machine has no approving user.
    let created = http(
        "POST",
        "/v1/machines",
        Some(owner()),
        Some(&json!({ "name": unique("ci") })),
    );
    assert_eq!(created.status, 201, "{}", created.body);
    let credential = created.json()["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    let admin = mint(&credential);
    assert_eq!(admin.status, 403, "{}", admin.body);
    assert_eq!(admin.json()["error"], "forbidden");

    // No credential, a session token and a made-up credential are all 401.
    for token in [None, Some(owner()), Some("smm_nope")] {
        let reply = http("POST", "/v1/devices/signin-links", token, Some(&json!({})));
        assert_eq!(reply.status, 401, "{}", reply.body);
    }

    // A revoked machine mints nothing, and the links it minted before die with it.
    let (machine_id, credential) = paired_machine();
    let code = code_of(mint(&credential).json()["url"].as_str().expect("url"));
    let revoked = http(
        "DELETE",
        &format!("/v1/devices/{machine_id}"),
        Some(owner()),
        None,
    );
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    assert_eq!(mint(&credential).status, 401);
    assert_eq!(redeem(&code).status, 410);
}

#[test]
fn a_link_session_can_be_revoked_like_any_other() {
    let (_, credential) = paired_machine();
    let code = code_of(mint(&credential).json()["url"].as_str().expect("url"));
    let session = redeem(&code).json();
    let token = session["token"].as_str().expect("token");
    assert_eq!(threads_status(token), 200);
    let revoked = http("DELETE", "/v1/auth/sessions/current", Some(token), None);
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    assert_eq!(threads_status(token), 401);
}

// ---- B: the browser device-code flow ------------------------------------------------------

struct Browser {
    device_code: String,
    user_code: String,
}

fn browser_start() -> Browser {
    let reply = http("POST", "/v1/auth/device/code", None, Some(&json!({})));
    assert_eq!(reply.status, 200, "device/code: {}", reply.body);
    let body = reply.json();
    assert_eq!(body["expires_in"], 600);
    assert_eq!(body["interval"], 5);
    let uri = body["verification_uri"].as_str().expect("verification_uri");
    assert_eq!(
        body["verification_uri_complete"],
        format!(
            "{uri}?user_code={}",
            body["user_code"].as_str().expect("code")
        )
    );
    Browser {
        device_code: body["device_code"].as_str().expect("device_code").into(),
        user_code: body["user_code"].as_str().expect("user_code").into(),
    }
}

fn browser_poll(device_code: &str) -> Reply {
    http(
        "POST",
        "/v1/auth/device/token",
        None,
        Some(&json!({ "device_code": device_code })),
    )
}

fn browser_approve(token: Option<&str>, user_code: &str) -> Reply {
    let _shared = APPROVE_LIMIT.read().unwrap_or_else(|e| e.into_inner());
    browser_approve_unlocked(token, user_code)
}

fn browser_approve_unlocked(token: Option<&str>, user_code: &str) -> Reply {
    http(
        "POST",
        "/v1/auth/device/approve",
        token,
        Some(&json!({ "user_code": user_code })),
    )
}

#[test]
fn the_browser_flow_gives_one_session_naming_the_approver_and_org() {
    let pairing = browser_start();
    assert!(pairing.device_code.starts_with("sdc_"));
    assert_eq!(pairing.user_code.len(), 9);

    let pending = browser_poll(&pairing.device_code);
    assert_eq!(pending.status, 400, "{}", pending.body);
    assert_eq!(pending.json()["error"], "authorization_pending");

    // Approval needs a signed-in member.
    let none = browser_approve(None, &pairing.user_code);
    assert_eq!(none.status, 401, "{}", none.body);
    let viewer = browser_approve(Some(&session("viewer")), &pairing.user_code);
    assert_eq!(viewer.status, 403, "{}", viewer.body);
    let approved = browser_approve(Some(owner()), &pairing.user_code);
    assert_eq!(approved.status, 200, "{}", approved.body);
    // A user code answers once.
    let twice = browser_approve(Some(owner()), &pairing.user_code);
    assert_eq!(twice.status, 404, "{}", twice.body);

    std::thread::sleep(INTERVAL);
    let granted = browser_poll(&pairing.device_code);
    assert_eq!(granted.status, 200, "{}", granted.body);
    let granted = granted.json();
    let token = granted["token"].as_str().expect("token").to_owned();
    assert!(token.starts_with("smt_"));
    assert_eq!(granted["user"]["display_name"], "Owner");
    assert_eq!(granted["org"]["name"], "Signin Test");
    assert_eq!(granted["role"], "member");
    assert_eq!(threads_status(&token), 200, "the session works");

    // Once: the same device code is spent.
    std::thread::sleep(INTERVAL);
    let spent = browser_poll(&pairing.device_code);
    assert_eq!(spent.status, 400, "{}", spent.body);
    assert_eq!(spent.json()["error"], "invalid_grant");
}

#[test]
fn browser_polling_errors_follow_rfc_8628() {
    let unknown = browser_poll("sdc_nothing");
    assert_eq!(unknown.status, 400, "{}", unknown.body);
    assert_eq!(unknown.json()["error"], "invalid_grant");
    let missing = http("POST", "/v1/auth/device/token", None, Some(&json!({})));
    assert_eq!(missing.status, 400, "{}", missing.body);

    let pairing = browser_start();
    assert_eq!(
        browser_poll(&pairing.device_code).json()["error"],
        "authorization_pending"
    );
    let slow = browser_poll(&pairing.device_code);
    assert_eq!(slow.status, 400, "{}", slow.body);
    let slow = slow.json();
    assert_eq!(slow["error"], "slow_down");
    assert_eq!(slow["interval"], 10);

    // A machine pairing's device code is not a browser code.
    let machine = start();
    assert_eq!(
        browser_poll(&machine.device_code).json()["error"],
        "invalid_grant"
    );
}

#[test]
fn guessing_browser_user_codes_is_limited_with_retry_after() {
    // Exclusive: the org's counter is shared with every other approval.
    let _exclusive = APPROVE_LIMIT.write().unwrap_or_else(|e| e.into_inner());
    let member = session("member");
    for attempt in 0..5 {
        let reply = browser_approve_unlocked(Some(&member), "ABCD-EFGH");
        assert_eq!(reply.status, 404, "attempt {attempt}: {}", reply.body);
    }
    let limited = browser_approve_unlocked(Some(&member), "ABCD-EFGH");
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert_eq!(limited.json()["error"], "rate_limited");
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After header")
        .parse()
        .expect("seconds");
    assert!((1..=60).contains(&wait), "Retry-After {wait}");
    // The machine-pairing approve shares the counters.
    let pairing = start();
    let shared = http(
        "POST",
        "/v1/devices/approve",
        Some(owner()),
        Some(&json!({ "user_code": pairing.user_code, "name": unique("laptop") })),
    );
    assert_eq!(shared.status, 429, "{}", shared.body);
    // Let the window pass so the other tests' approvals work again.
    std::thread::sleep(Duration::from_secs(wait + 1));
}
