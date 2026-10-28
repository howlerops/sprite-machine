//! M2B-005 live tests of device-code pairing against a running hub (`wrangler dev --local`):
//! RFC 8628 start, approve by a member, the runner's polling (authorization_pending, slow_down,
//! access_denied), the single-use machine credential, the registry (list, revoke closing the
//! tunnel with `auth_revoked`) and the `require_trusted_devices` setting.
//!
//! Needs `HUB_URL`, a hub with a `BOOTSTRAP_OWNER_TOKEN` var and the same token in this
//! process's environment. `bash scripts/hub-test.sh devices` does all of that. Without them every
//! test fails with a message saying so; nothing passes vacuously. The `live` feature keeps this
//! file out of a plain `cargo test -p hub`. The 10-minute expiry and the slow_down arithmetic
//! need a clock the live hub cannot move: they are native tests in `src/devices/tests.rs`.
//!
//! The one bootstrap per hub is done once, lazily, and its owner session approves the devices.

use std::io::{Read, Write};
use std::net::TcpStream;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{OnceLock, RwLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

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
             Run it with `bash scripts/hub-test.sh devices`."
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
            "BOOTSTRAP_OWNER_TOKEN is not set: run this test with `bash scripts/hub-test.sh devices`."
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
            Some(&json!({ "org_name": "Devices Test", "user_name": "Owner" })),
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

// ---- a minimal tunnel WebSocket ----------------------------------------------------------

struct Ws {
    stream: TcpStream,
    buf: Vec<u8>,
}

impl Ws {
    fn open_tunnel(machine: &str, credential: &str) -> Self {
        Self::try_open_tunnel(machine, credential)
            .unwrap_or_else(|head| panic!("tunnel refused: {head}"))
    }

    /// The upgrade, or the response head when it was refused.
    fn try_open_tunnel(machine: &str, credential: &str) -> Result<Self, String> {
        let mut stream = connect();
        let request = format!(
            "GET /v1/machines/{machine}/tunnel HTTP/1.1\r\nHost: {}\r\nUpgrade: websocket\r\n\
             Connection: Upgrade\r\nSec-WebSocket-Key: dGhlIHNhbXBsZSBub25jZQ==\r\n\
             Sec-WebSocket-Version: 13\r\nSec-WebSocket-Protocol: sm.tunnel.v1\r\n\
             Authorization: Bearer {credential}\r\n\r\n",
            authority()
        );
        stream.write_all(request.as_bytes()).expect("send upgrade");
        let mut head = Vec::new();
        let mut byte = [0u8; 1];
        while !head.ends_with(b"\r\n\r\n") {
            match stream.read(&mut byte) {
                Ok(1) => head.push(byte[0]),
                other => panic!("connection ended during the upgrade ({other:?}): {head:?}"),
            }
        }
        let head = String::from_utf8_lossy(&head).into_owned();
        if !head.starts_with("HTTP/1.1 101") {
            return Err(head);
        }
        Ok(Self {
            stream,
            buf: Vec::new(),
        })
    }

    fn send_text(&mut self, text: &str) {
        let payload = text.as_bytes();
        let mask = [1u8, 2, 3, 4];
        let mut frame = vec![0x81u8];
        if payload.len() < 126 {
            frame.push(0x80 | payload.len() as u8);
        } else {
            frame.push(0x80 | 126);
            frame.extend_from_slice(&(payload.len() as u16).to_be_bytes());
        }
        frame.extend_from_slice(&mask);
        frame.extend(payload.iter().enumerate().map(|(i, b)| b ^ mask[i % 4]));
        self.stream.write_all(&frame).expect("send frame");
    }

    /// The next text frame as JSON.
    fn json(&mut self) -> Value {
        let deadline = Instant::now() + READ_TIMEOUT;
        loop {
            if let Some((opcode, payload, used)) = parse_frame(&self.buf) {
                self.buf.drain(..used);
                if opcode == 0x1 {
                    return serde_json::from_slice(&payload).expect("frame is JSON");
                }
                assert_ne!(opcode, 0x8, "the server closed the WebSocket");
                continue;
            }
            assert!(Instant::now() < deadline, "no WebSocket frame in time");
            let mut chunk = [0u8; 4096];
            match self.stream.read(&mut chunk) {
                Ok(0) => panic!("the server closed the connection"),
                Ok(n) => self.buf.extend_from_slice(&chunk[..n]),
                Err(error) => panic!("read: {error}"),
            }
        }
    }
}

fn parse_frame(buf: &[u8]) -> Option<(u8, Vec<u8>, usize)> {
    if buf.len() < 2 {
        return None;
    }
    let opcode = buf[0] & 0x0f;
    let mut len = usize::from(buf[1] & 0x7f);
    let mut offset = 2;
    if len == 126 {
        let bytes = buf.get(2..4)?;
        len = usize::from(u16::from_be_bytes([bytes[0], bytes[1]]));
        offset = 4;
    }
    let payload = buf.get(offset..offset + len)?;
    Some((opcode, payload.to_vec(), offset + len))
}

fn hello(machine: &str) -> Value {
    json!({
        "t": "hello", "proto": 1, "machine_id": machine, "runner_version": "0.0.0-test",
        "resume_token": null, "last_down_seq": 0,
        "capabilities": {
            "host_mode": "local", "harnesses": ["claude"], "max_sessions": 4,
            "spawn": ["same_dir"], "features": [],
        },
    })
}

// ---- tests ------------------------------------------------------------------------------

#[test]
fn code_start_has_the_rfc_8628_fields() {
    let reply = http("POST", "/v1/devices/code", None, Some(&json!({})));
    assert_eq!(reply.status, 200, "{}", reply.body);
    let body = reply.json();
    let device_code = body["device_code"].as_str().expect("device_code");
    assert!(device_code.starts_with("sdc_") && device_code.len() == 68);
    let user_code = body["user_code"].as_str().expect("user_code");
    assert_eq!(user_code.len(), 9, "{user_code}");
    assert_eq!(&user_code[4..5], "-");
    assert!(
        !user_code.contains(['0', 'O', '1', 'I', 'L']),
        "ambiguous characters in {user_code}"
    );
    assert_eq!(body["expires_in"], 600);
    assert_eq!(body["interval"], 5);
    let uri = body["verification_uri"].as_str().expect("verification_uri");
    assert!(uri.ends_with("/device"), "{uri}");
    assert_eq!(
        body["verification_uri_complete"],
        format!("{uri}?user_code={user_code}")
    );
    // Two starts never share a code.
    let other = start();
    assert_ne!(other.device_code, device_code);
    assert_ne!(other.user_code, user_code);
}

#[test]
fn pending_then_approved_gives_one_credential_and_the_registry_reuses_machines() {
    let pairing = start();

    let pending = poll(&pairing.device_code);
    assert_eq!(pending.status, 400, "{}", pending.body);
    assert_eq!(pending.json()["error"], "authorization_pending");

    // Approving needs a session, and at least the member role.
    let anonymous = approve(None, &pairing.user_code, "laptop");
    assert_eq!(anonymous.status, 401, "{}", anonymous.body);
    let viewer = approve(Some(&session("viewer")), &pairing.user_code, "laptop");
    assert_eq!(viewer.status, 403, "{}", viewer.body);
    let unknown = approve(Some(&session("member")), "ABCD-EFGH", "laptop");
    assert_eq!(unknown.status, 404, "{}", unknown.body);

    // Honour the interval, then a member approves (lower case and no dash are accepted).
    std::thread::sleep(INTERVAL);
    let typed = pairing.user_code.replace('-', "").to_lowercase();
    let name = unique("laptop");
    let approved = approve(Some(&session("member")), &typed, &name);
    assert_eq!(approved.status, 200, "{}", approved.body);

    let granted = poll(&pairing.device_code);
    assert_eq!(granted.status, 200, "{}", granted.body);
    let granted = granted.json();
    let machine_id = granted["machine_id"]
        .as_str()
        .expect("machine_id")
        .to_owned();
    let credential = granted["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    assert!(credential.starts_with("smm_") && credential.len() == 68);

    // Single use: the code is spent, whoever asks.
    std::thread::sleep(INTERVAL);
    let again = poll(&pairing.device_code);
    assert_eq!(again.status, 400, "{}", again.body);
    assert_eq!(again.json()["error"], "invalid_grant");
    assert!(!again.body.contains(&credential));

    // The paired machine is a normal registry row, and a listed trusted device.
    let machines = http("GET", "/v1/machines", Some(owner()), None).json();
    let in_registry = machines["machines"]
        .as_array()
        .expect("machines")
        .iter()
        .any(|m| m["machine_id"] == machine_id.as_str() && m["name"] == name.as_str());
    assert!(in_registry, "{machines}");
    let listed = http("GET", "/v1/devices", Some(owner()), None);
    assert_eq!(listed.status, 200, "{}", listed.body);
    let listed = listed.json();
    let row = listed["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["machine_id"] == machine_id.as_str())
        .cloned()
        .expect("device in the list");
    assert_eq!(row["name"], name.as_str());
    assert_eq!(row["revoked_at"], Value::Null);
    assert!(!listed.to_string().contains(&credential));
    assert_eq!(
        http("GET", "/v1/devices", None, None).status,
        401,
        "the list needs a session"
    );

    // The credential opens the tunnel; revoking the device closes it with auth_revoked.
    let mut ws = Ws::open_tunnel(&machine_id, &credential);
    ws.send_text(&hello(&machine_id).to_string());
    assert_eq!(ws.json()["t"], "welcome");
    let denied = http(
        "DELETE",
        &format!("/v1/devices/{machine_id}"),
        Some(&session("member")),
        None,
    );
    assert_eq!(denied.status, 403, "{}", denied.body);
    let revoked = http(
        "DELETE",
        &format!("/v1/devices/{machine_id}"),
        Some(owner()),
        None,
    );
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    let close = ws.json();
    assert_eq!(
        (close["t"].as_str(), close["code"].as_str()),
        (Some("close"), Some("auth_revoked"))
    );
    let listed = http("GET", "/v1/devices", Some(owner()), None).json();
    let row = listed["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["machine_id"] == machine_id.as_str())
        .cloned()
        .expect("device still listed");
    assert!(row["revoked_at"].is_string(), "{row}");
}

#[test]
fn polling_faster_than_the_interval_is_slow_down() {
    let pairing = start();
    let first = poll(&pairing.device_code);
    assert_eq!(first.json()["error"], "authorization_pending");
    let second = poll(&pairing.device_code);
    assert_eq!(second.status, 400, "{}", second.body);
    let body = second.json();
    assert_eq!(body["error"], "slow_down");
    assert_eq!(body["interval"], 10, "the interval grows by 5 s");
    // Waiting out the old 5 s is not enough any more.
    std::thread::sleep(INTERVAL);
    assert_eq!(poll(&pairing.device_code).json()["error"], "slow_down");
}

#[test]
fn a_denied_pairing_is_access_denied_and_never_a_machine() {
    let pairing = start();
    let denied = http(
        "POST",
        "/v1/devices/deny",
        Some(&session("member")),
        Some(&json!({ "user_code": pairing.user_code })),
    );
    assert_eq!(denied.status, 200, "{}", denied.body);
    let reply = poll(&pairing.device_code);
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.json()["error"], "access_denied");
    // It cannot be approved afterwards.
    let late = approve(Some(&session("member")), &pairing.user_code, "laptop");
    assert_eq!(late.status, 404, "{}", late.body);
}

#[test]
fn an_unknown_device_code_is_invalid_grant() {
    let reply = poll(&format!("sdc_{}", "0".repeat(64)));
    assert_eq!(reply.status, 400, "{}", reply.body);
    assert_eq!(reply.json()["error"], "invalid_grant");
    let missing = http("POST", "/v1/devices/token", None, Some(&json!({})));
    assert_eq!(missing.status, 400, "{}", missing.body);
}

#[test]
fn the_trusted_devices_setting_is_admin_only_and_keeps_admin_machines_allowed() {
    let member = session("member");
    let admin_denied = http(
        "PUT",
        "/v1/devices/policy",
        Some(&member),
        Some(&json!({ "require_trusted_devices": true })),
    );
    assert_eq!(admin_denied.status, 403, "{}", admin_denied.body);
    let bad = http(
        "PUT",
        "/v1/devices/policy",
        Some(owner()),
        Some(&json!({ "require_trusted_devices": "yes" })),
    );
    assert_eq!(bad.status, 400, "{}", bad.body);

    // An admin-API machine exists before the setting is on ...
    let created = http(
        "POST",
        "/v1/machines",
        Some(owner()),
        Some(&json!({ "name": unique("ci") })),
    );
    assert_eq!(created.status, 201, "{}", created.body);
    let created = created.json();
    let machine_id = created["machine"]["id"].as_str().expect("id").to_owned();
    let credential = created["credential"]
        .as_str()
        .expect("credential")
        .to_owned();

    let on = http(
        "PUT",
        "/v1/devices/policy",
        Some(owner()),
        Some(&json!({ "require_trusted_devices": true })),
    );
    assert_eq!(on.status, 200, "{}", on.body);
    let listed = http("GET", "/v1/devices", Some(owner()), None).json();
    assert_eq!(listed["require_trusted_devices"], true);

    // ... and still connects with it on (admin-issued credentials stay allowed), while it is not
    // listed as a paired device and cannot be revoked through the device route.
    let mut ws = Ws::open_tunnel(&machine_id, &credential);
    ws.send_text(&hello(&machine_id).to_string());
    assert_eq!(ws.json()["t"], "welcome");
    assert!(
        !listed["devices"]
            .as_array()
            .expect("devices")
            .iter()
            .any(|d| d["machine_id"] == machine_id.as_str())
    );
    let not_a_device = http(
        "DELETE",
        &format!("/v1/devices/{machine_id}"),
        Some(owner()),
        None,
    );
    assert_eq!(not_a_device.status, 404, "{}", not_a_device.body);

    // A new pairing still completes with the setting on: that is the trusted path.
    let pairing = start();
    let approved = approve(Some(owner()), &pairing.user_code, &unique("trusted"));
    assert_eq!(approved.status, 200, "{}", approved.body);
    let granted = poll(&pairing.device_code);
    assert_eq!(granted.status, 200, "{}", granted.body);

    let off = http(
        "PUT",
        "/v1/devices/policy",
        Some(owner()),
        Some(&json!({ "require_trusted_devices": false })),
    );
    assert_eq!(off.status, 200, "{}", off.body);
}

#[test]
fn the_tunnel_refuses_a_machine_that_is_not_vouched_for_when_the_org_requires_trusted_devices() {
    // A machine the dev credential route provisions has a Machine DO but no machines row, so no
    // approved pairing or admin vouches for it. Its org is the dev org (`Bearer dev`).
    let machine = unique("unvouched");
    let created = http(
        "POST",
        &format!("/v1/dev/machines/{machine}/credential"),
        Some("dev"),
        None,
    );
    assert_eq!(created.status, 201, "{}", created.body);
    let credential = created.json()["credential"]
        .as_str()
        .expect("credential")
        .to_owned();
    let policy = |on: bool| {
        let reply = http(
            "PUT",
            "/v1/devices/policy",
            Some("dev"),
            Some(&json!({ "require_trusted_devices": on })),
        );
        assert_eq!(reply.status, 200, "{}", reply.body);
    };

    // Setting off: it connects.
    policy(false);
    drop(Ws::open_tunnel(&machine, &credential));

    // Setting on: refused before the upgrade, the status of a missing credential (tunnel.md §2).
    policy(true);
    let refused = Ws::try_open_tunnel(&machine, &credential)
        .err()
        .expect("an unvouched machine must be refused");
    assert!(refused.starts_with("HTTP/1.1 401"), "{refused}");

    // Admin-issued and device-paired machines of an org with the setting on still connect.
    let admin = http(
        "POST",
        "/v1/machines",
        Some("dev"),
        Some(&json!({ "name": unique("ci") })),
    );
    assert_eq!(admin.status, 201, "{}", admin.body);
    let admin = admin.json();
    let admin_id = admin["machine"]["id"].as_str().expect("id").to_owned();
    let admin_credential = admin["credential"].as_str().expect("credential").to_owned();
    let mut ws = Ws::open_tunnel(&admin_id, &admin_credential);
    ws.send_text(&hello(&admin_id).to_string());
    assert_eq!(ws.json()["t"], "welcome");

    let pairing = start();
    let approved = approve(Some("dev"), &pairing.user_code, &unique("trusted"));
    assert_eq!(approved.status, 200, "{}", approved.body);
    let granted = poll(&pairing.device_code).json();
    let device_id = granted["machine_id"].as_str().expect("id").to_owned();
    let device_credential = granted["credential"].as_str().expect("credential");
    let mut ws = Ws::open_tunnel(&device_id, device_credential);
    ws.send_text(&hello(&device_id).to_string());
    assert_eq!(ws.json()["t"], "welcome");

    policy(false);
    drop(Ws::open_tunnel(&machine, &credential));
}

#[test]
fn guessing_user_codes_is_limited_per_user_and_org_with_retry_after() {
    // Exclusive: the org's counter is shared with every other test's approvals.
    let _exclusive = APPROVE_LIMIT.write().unwrap_or_else(|e| e.into_inner());
    let member = session("member");
    let miss = |token: &str| {
        http(
            "POST",
            "/v1/devices/approve",
            Some(token),
            Some(&json!({ "user_code": "ABCD-EFGH", "name": "laptop" })),
        )
    };
    for attempt in 0..5 {
        let reply = miss(&member);
        assert_eq!(reply.status, 404, "attempt {attempt}: {}", reply.body);
    }
    let limited = miss(&member);
    assert_eq!(limited.status, 429, "{}", limited.body);
    assert_eq!(limited.json()["error"], "rate_limited");
    let wait: u64 = limited
        .header("retry-after")
        .expect("Retry-After header")
        .parse()
        .expect("seconds");
    assert!((1..=60).contains(&wait), "Retry-After {wait}");

    // The org's counter is full: another user of the org is limited too, even with a real code.
    let pairing = start();
    let other = approve_unlocked(&session("member"), &pairing.user_code);
    assert_eq!(other.status, 429, "{}", other.body);

    // Let the one-minute window pass so the other tests' approvals work again.
    std::thread::sleep(Duration::from_secs(wait + 1));
    let again = approve_unlocked(&session("member"), &pairing.user_code);
    assert_eq!(again.status, 200, "{}", again.body);
}

fn approve_unlocked(token: &str, user_code: &str) -> Reply {
    http(
        "POST",
        "/v1/devices/approve",
        Some(token),
        Some(&json!({ "user_code": user_code, "name": unique("laptop") })),
    )
}

#[test]
fn the_token_names_the_org_and_approver_and_a_machine_can_revoke_itself() {
    let pairing = start();
    let approved = approve(Some(owner()), &pairing.user_code, &unique("laptop"));
    assert_eq!(approved.status, 200, "{}", approved.body);
    let granted = poll(&pairing.device_code);
    assert_eq!(granted.status, 200, "{}", granted.body);
    let granted = granted.json();
    assert_eq!(granted["org"]["name"], "Devices Test");
    assert!(
        granted["org"]["id"]
            .as_str()
            .is_some_and(|id| !id.is_empty())
    );
    assert_eq!(granted["org"]["id"], granted["org_id"]);
    assert_eq!(granted["approved_by"]["name"], "Owner");
    assert!(granted["approved_by"]["id"].as_str().is_some());
    let machine_id = granted["machine_id"].as_str().expect("id").to_owned();
    let credential = granted["credential"]
        .as_str()
        .expect("credential")
        .to_owned();

    // A session token is not a machine credential; neither is a made-up one.
    let session_token = http("DELETE", "/v1/devices/self", Some(owner()), None);
    assert_eq!(session_token.status, 401, "{}", session_token.body);
    let none = http("DELETE", "/v1/devices/self", None, None);
    assert_eq!(none.status, 401, "{}", none.body);

    let revoked = http("DELETE", "/v1/devices/self", Some(&credential), None);
    assert_eq!(revoked.status, 200, "{}", revoked.body);
    let listed = http("GET", "/v1/devices", Some(owner()), None).json();
    let mine = listed["devices"]
        .as_array()
        .expect("devices")
        .iter()
        .find(|d| d["machine_id"] == machine_id.as_str())
        .expect("listed");
    assert!(mine["revoked_at"].is_string(), "{mine}");
    // The credential is spent for this route too.
    let again = http("DELETE", "/v1/devices/self", Some(&credential), None);
    assert_eq!(again.status, 401, "{}", again.body);
}
