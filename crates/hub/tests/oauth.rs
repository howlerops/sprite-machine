//! M2-003b live tests of GitHub OAuth (authorization code + PKCE), members and invites against a
//! running hub (`wrangler dev --local`) and a tiny mock GitHub served by this test process.
//!
//! The hub is started with `GITHUB_API_BASE` pointing at the mock (authorize, token and user
//! endpoints), a test `GITHUB_CLIENT_ID` and a random per-run `GITHUB_CLIENT_SECRET`. The mock
//! checks the client id, the secret, the code, the redirect URI and the PKCE verifier exactly as
//! GitHub would, so a hub that skips any of them fails here. The test plays the browser: it follows
//! each redirect by hand, so it can see where the hub sends it.
//!
//! Needs `HUB_URL`, `BOOTSTRAP_OWNER_TOKEN`, `GITHUB_CLIENT_ID`, `GITHUB_CLIENT_SECRET` and
//! `MOCK_GITHUB_PORT` in the environment: `bash scripts/hub-test.sh oauth` sets all of them. Without
//! them every test fails with a message saying so; nothing passes vacuously. The `live` feature keeps
//! this file out of a plain `cargo test -p hub`. Expiry of the state and the code needs a clock the
//! live hub cannot move: those are native tests in `src/auth/github_tests.rs`.
//!
//! The one bootstrap per hub is done once, lazily, and its owner session invites the others.

use std::collections::HashMap;
use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use hub::auth::github::{parse_query, pct_encode};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

const READ_TIMEOUT: Duration = Duration::from_secs(20);

fn env(name: &str) -> String {
    match std::env::var(name) {
        Ok(value) if !value.trim().is_empty() => value,
        _ => panic!("{name} is not set: run this test with `bash scripts/hub-test.sh oauth`."),
    }
}

// ---- plain HTTP -------------------------------------------------------------------------

struct Reply {
    status: u16,
    head: String,
    body: String,
}

impl Reply {
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

    fn location(&self) -> String {
        assert_eq!(
            self.status, 302,
            "expected a redirect, got {}: {}",
            self.status, self.body
        );
        self.header("location").expect("Location header")
    }
}

fn hub_authority() -> String {
    let url = env("HUB_URL");
    url.trim()
        .trim_end_matches('/')
        .strip_prefix("http://")
        .unwrap_or_else(|| panic!("HUB_URL must look like http://host:port, got {url}"))
        .to_owned()
}

fn mock_authority() -> String {
    format!("127.0.0.1:{}", env("MOCK_GITHUB_PORT"))
}

fn http_to(
    authority: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
) -> Reply {
    http_with(authority, method, path, token, body, &[])
}

fn http_with(
    authority: &str,
    method: &str,
    path: &str,
    token: Option<&str>,
    body: Option<&Value>,
    headers: &[(&str, &str)],
) -> Reply {
    let mut stream = TcpStream::connect(authority)
        .unwrap_or_else(|error| panic!("cannot connect to {authority}: {error}"));
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("timeout");
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

fn http(method: &str, path: &str, token: Option<&str>, body: Option<&Value>) -> Reply {
    // wrangler dev sets `CF-Connecting-IP` on every request, and the authorize start is limited
    // per IP: give each call its own address so only the rate-limit test meets the limit.
    static CALLS: AtomicU64 = AtomicU64::new(0);
    let n = CALLS.fetch_add(1, Ordering::Relaxed) + 1;
    let ip = format!("10.{}.{}.{}", (n >> 16) & 255, (n >> 8) & 255, n & 255);
    http_with(
        &hub_authority(),
        method,
        path,
        token,
        body,
        &[("CF-Connecting-IP", &ip)],
    )
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
    Reply {
        status,
        head: head.to_owned(),
        body: if chunked {
            dechunk(body)
        } else {
            body.to_owned()
        },
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

// ---- the mock GitHub ------------------------------------------------------------------------

struct MockCode {
    challenge: String,
    redirect_uri: String,
    login: String,
    id: i64,
    used: bool,
}

#[derive(Default)]
struct Mock {
    counter: u64,
    codes: HashMap<String, MockCode>,
    tokens: HashMap<String, (i64, String)>,
    /// Every token-endpoint form the hub sent (for assertions about what it sends).
    token_forms: Vec<HashMap<String, String>>,
}

fn mock() -> &'static Arc<Mutex<Mock>> {
    static MOCK: OnceLock<Arc<Mutex<Mock>>> = OnceLock::new();
    MOCK.get_or_init(|| {
        let listener = TcpListener::bind(mock_authority())
            .unwrap_or_else(|error| panic!("cannot bind the mock GitHub: {error}"));
        let state = Arc::new(Mutex::new(Mock::default()));
        let shared = Arc::clone(&state);
        std::thread::spawn(move || {
            for stream in listener.incoming().flatten() {
                let shared = Arc::clone(&shared);
                std::thread::spawn(move || serve_mock(stream, &shared));
            }
        });
        state
    })
}

fn b64_sha256(value: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(value.as_bytes()))
}

fn read_request(stream: &mut TcpStream) -> (String, String, HashMap<String, String>, String) {
    stream
        .set_read_timeout(Some(READ_TIMEOUT))
        .expect("timeout");
    let mut raw = Vec::new();
    let mut buf = [0u8; 4096];
    let head_end = loop {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break raw.len();
        }
        raw.extend_from_slice(&buf[..n]);
        if let Some(pos) = raw.windows(4).position(|w| w == b"\r\n\r\n") {
            break pos + 4;
        }
    };
    let head = String::from_utf8_lossy(&raw[..head_end.min(raw.len())]).into_owned();
    let mut lines = head.split("\r\n");
    let first = lines.next().unwrap_or_default().to_owned();
    let headers: HashMap<String, String> = lines
        .filter_map(|line| line.split_once(':'))
        .map(|(k, v)| (k.trim().to_ascii_lowercase(), v.trim().to_owned()))
        .collect();
    let want: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    while raw.len() < head_end + want {
        let n = stream.read(&mut buf).unwrap_or(0);
        if n == 0 {
            break;
        }
        raw.extend_from_slice(&buf[..n]);
    }
    let body = String::from_utf8_lossy(&raw[head_end.min(raw.len())..]).into_owned();
    let mut parts = first.split_whitespace();
    let method = parts.next().unwrap_or_default().to_owned();
    let target = parts.next().unwrap_or_default().to_owned();
    (method, target, headers, body)
}

fn mock_reply(stream: &mut TcpStream, status: &str, extra: &str, body: &str) {
    let response = format!(
        "HTTP/1.1 {status}\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{body}",
        body.len()
    );
    let _ = stream.write_all(response.as_bytes());
}

fn serve_mock(mut stream: TcpStream, state: &Mutex<Mock>) {
    let (method, target, headers, body) = read_request(&mut stream);
    let (path, query) = target.split_once('?').unwrap_or((&target, ""));
    let query: HashMap<String, String> = parse_query(query).into_iter().collect();
    let mut mock = state.lock().unwrap_or_else(|e| e.into_inner());
    match (method.as_str(), path) {
        // The "browser leg": the test appends `as=<login>:<id>`, standing in for the user who
        // logs in at GitHub and approves.
        ("GET", "/login/oauth/authorize") => {
            let known = query.get("client_id").map(String::as_str)
                == Some(env("GITHUB_CLIENT_ID").as_str())
                && query.get("code_challenge_method").map(String::as_str) == Some("S256")
                && query.contains_key("code_challenge")
                && query.contains_key("state")
                && query.contains_key("redirect_uri");
            let identity = query
                .get("as")
                .and_then(|v| v.split_once(':'))
                .and_then(|(login, id)| Some((login.to_owned(), id.parse::<i64>().ok()?)));
            let (true, Some((login, id))) = (known, identity) else {
                return mock_reply(
                    &mut stream,
                    "400 Bad Request",
                    "",
                    r#"{"error":"bad authorize request"}"#,
                );
            };
            mock.counter += 1;
            let code = format!("mockcode{}", mock.counter);
            let redirect_uri = query["redirect_uri"].clone();
            mock.codes.insert(
                code.clone(),
                MockCode {
                    challenge: query["code_challenge"].clone(),
                    redirect_uri: redirect_uri.clone(),
                    login,
                    id,
                    used: false,
                },
            );
            let location = format!(
                "{redirect_uri}?code={code}&state={}",
                pct_encode(&query["state"])
            );
            mock_reply(
                &mut stream,
                "302 Found",
                &format!("Location: {location}\r\n"),
                "",
            );
        }
        ("POST", "/login/oauth/access_token") => {
            let form: HashMap<String, String> = parse_query(&body).into_iter().collect();
            mock.token_forms.push(form.clone());
            let fail = |stream: &mut TcpStream, why: &str| {
                mock_reply(stream, "200 OK", "", &json!({ "error": why }).to_string());
            };
            let get = |key: &str| form.get(key).map(String::as_str).unwrap_or_default();
            if get("client_id") != env("GITHUB_CLIENT_ID")
                || get("client_secret") != env("GITHUB_CLIENT_SECRET")
            {
                return fail(&mut stream, "incorrect_client_credentials");
            }
            let Some(entry) = mock.codes.get_mut(get("code")) else {
                return fail(&mut stream, "bad_verification_code");
            };
            if entry.used
                || entry.redirect_uri != get("redirect_uri")
                || b64_sha256(get("code_verifier")) != entry.challenge
            {
                entry.used = true;
                return fail(&mut stream, "bad_verification_code");
            }
            entry.used = true;
            let (id, login) = (entry.id, entry.login.clone());
            mock.counter += 1;
            let token = format!("gho_mock{}", mock.counter);
            mock.tokens.insert(token.clone(), (id, login));
            mock_reply(
                &mut stream,
                "200 OK",
                "",
                &json!({ "access_token": token, "token_type": "bearer", "scope": "read:user" })
                    .to_string(),
            );
        }
        ("GET", "/user") => {
            let bearer = headers
                .get("authorization")
                .and_then(|v| v.strip_prefix("Bearer "))
                .unwrap_or_default();
            match mock.tokens.get(bearer) {
                Some((id, login)) => mock_reply(
                    &mut stream,
                    "200 OK",
                    "",
                    &json!({ "id": id, "login": login, "name": format!("{login} (mock)"), "email": null }).to_string(),
                ),
                None => mock_reply(&mut stream, "401 Unauthorized", "", r#"{"message":"Bad credentials"}"#),
            }
        }
        _ => mock_reply(&mut stream, "404 Not Found", "", "{}"),
    }
}

// ---- the app side: PKCE and the redirects ---------------------------------------------------

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

fn next_id() -> i64 {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let base = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .expect("clock")
        .as_secs() as i64
        * 1000;
    base + NEXT.fetch_add(1, Ordering::Relaxed) as i64
}

fn loopback() -> String {
    format!("http://127.0.0.1:{}/callback", 40_000 + next_id() % 20_000)
}

/// A PKCE verifier (64 hex characters) and its S256 challenge.
fn pkce() -> (String, String) {
    let verifier = hex(&Sha256::digest(unique("verifier").as_bytes()));
    let challenge = b64_sha256(&verifier);
    (verifier, challenge)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn authorize_path(redirect_uri: &str, challenge: &str, state: &str) -> String {
    format!(
        "/v1/auth/github/authorize?redirect_uri={}&code_challenge={challenge}&code_challenge_method=S256&state={state}",
        pct_encode(redirect_uri)
    )
}

/// What the app ends up with after the browser dance.
struct Landing {
    location: String,
    /// The hub path GitHub's redirect hit (so a test can replay it).
    callback_path: String,
}

impl Landing {
    fn param(&self, key: &str) -> Option<String> {
        let (_, query) = self.location.split_once('?')?;
        parse_query(query)
            .into_iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v)
    }
}

/// Plays the browser: hub authorize -> (mock) GitHub login as `login:id` -> hub callback -> app.
fn browse(redirect_uri: &str, challenge: &str, state: &str, login: &str, id: i64) -> Landing {
    let _ = mock();
    let to_github = http(
        "GET",
        &authorize_path(redirect_uri, challenge, state),
        None,
        None,
    )
    .location();
    assert!(
        to_github.starts_with(&format!(
            "http://{}/login/oauth/authorize?",
            mock_authority()
        )),
        "{to_github}"
    );
    assert!(
        !to_github.contains(&env("GITHUB_CLIENT_SECRET")),
        "the secret must never be in a URL"
    );
    assert!(to_github.contains("code_challenge_method=S256"));
    let github_path = to_github
        .strip_prefix(&format!("http://{}", mock_authority()))
        .expect("mock url");
    let back = http_to(
        &mock_authority(),
        "GET",
        &format!("{github_path}&as={login}:{id}"),
        None,
        None,
    )
    .location();
    let callback_path = back
        .strip_prefix(&format!("http://{}", hub_authority()))
        .expect("hub callback")
        .to_owned();
    assert!(
        callback_path.starts_with("/auth/github/callback?"),
        "{back}"
    );
    let location = http("GET", &callback_path, None, None).location();
    Landing {
        location,
        callback_path,
    }
}

fn exchange(code: &str, verifier: &str, redirect_uri: &str) -> Reply {
    http(
        "POST",
        "/v1/auth/github/token",
        None,
        Some(&json!({ "code": code, "code_verifier": verifier, "redirect_uri": redirect_uri })),
    )
}

/// Signs `login:id` in from the desktop app; returns the session reply.
fn desktop_sign_in(login: &str, id: i64) -> Reply {
    let redirect_uri = loopback();
    let (verifier, challenge) = pkce();
    let state = unique("appstate");
    let landing = browse(&redirect_uri, &challenge, &state, login, id);
    assert!(
        landing.location.starts_with(&redirect_uri),
        "{}",
        landing.location
    );
    assert_eq!(landing.param("state").as_deref(), Some(state.as_str()));
    let Some(code) = landing.param("code") else {
        panic!("no code in {}", landing.location);
    };
    exchange(&code, &verifier, &redirect_uri)
}

// ---- the org ---------------------------------------------------------------------------------

fn owner() -> &'static str {
    static OWNER: OnceLock<String> = OnceLock::new();
    OWNER.get_or_init(|| {
        let reply = http(
            "POST",
            "/v1/auth/bootstrap",
            Some(&env("BOOTSTRAP_OWNER_TOKEN")),
            Some(&json!({ "org_name": "OAuth Test", "user_name": "Owner" })),
        );
        assert_eq!(reply.status, 201, "bootstrap: {}", reply.body);
        reply.json()["session"]["token"]
            .as_str()
            .expect("owner token")
            .to_owned()
    })
}

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

fn invite(token: &str, login: &str, role: Option<&str>) -> Reply {
    let body = match role {
        Some(role) => json!({ "github_login": login, "role": role }),
        None => json!({ "github_login": login }),
    };
    http("POST", "/v1/invites", Some(token), Some(&body))
}

fn token_of(reply: &Reply) -> String {
    assert_eq!(reply.status, 201, "sign-in: {}", reply.body);
    reply.json()["token"].as_str().expect("token").to_owned()
}

fn audit_actions() -> Vec<(String, String)> {
    let reply = http("GET", "/v1/dev/audit?limit=1000", Some(owner()), None);
    assert_eq!(reply.status, 200, "audit: {}", reply.body);
    reply.json()["entries"]
        .as_array()
        .expect("entries")
        .iter()
        .map(|e| {
            (
                e["action"].as_str().unwrap_or_default().to_owned(),
                e.to_string(),
            )
        })
        .collect()
}

// ---- tests -----------------------------------------------------------------------------------

#[test]
fn the_desktop_pkce_flow_against_a_mocked_github_gives_a_working_session() {
    let login = unique("alice");
    let id = next_id();
    assert_eq!(invite(owner(), &login, None).status, 201);

    let reply = desktop_sign_in(&login, id);
    let body = reply.json();
    let token = token_of(&reply);
    assert!(token.starts_with("smt_"));
    assert_eq!(body["role"], json!("member"));
    assert!(body["user"]["id"].as_str().is_some());

    // The mock saw the exchange the way GitHub would: the secret and a verifier that matched.
    {
        let mock = mock().lock().unwrap_or_else(|e| e.into_inner());
        let form = mock.token_forms.last().expect("a token request");
        assert_eq!(
            form.get("client_secret"),
            Some(&env("GITHUB_CLIENT_SECRET"))
        );
        assert_eq!(form.get("client_id"), Some(&env("GITHUB_CLIENT_ID")));
        assert!(form["redirect_uri"].ends_with("/auth/github/callback"));
        assert!(form["code_verifier"].len() >= 43);
    }

    // The session is a real hub session: a member reads the member list, which now names them.
    let members = http("GET", "/v1/members", Some(&token), None);
    assert_eq!(members.status, 200, "{}", members.body);
    let listed = members.json();
    assert!(
        listed["members"]
            .as_array()
            .unwrap()
            .iter()
            .any(|m| m["github_login"] == json!(login) && m["role"] == json!("member")),
        "{listed}"
    );
    // A member is not an admin.
    assert_eq!(http("GET", "/v1/invites", Some(&token), None).status, 403);
    assert_eq!(
        http("DELETE", "/v1/auth/sessions/current", Some(&token), None).status,
        200
    );
    assert_eq!(http("GET", "/v1/members", Some(&token), None).status, 401);

    // Audited, and no secret, token or code in any row.
    let rows = audit_actions();
    assert!(
        rows.iter()
            .any(|(a, row)| a == "auth.github.signin" && row.contains(&login))
    );
    assert!(
        rows.iter()
            .any(|(a, row)| a == "member.invite.accept" && row.contains(&login))
    );
    assert!(
        rows.iter()
            .all(|(_, row)| !row.contains(&env("GITHUB_CLIENT_SECRET"))
                && !row.contains("smt_")
                && !row.contains("oac_"))
    );
}

#[test]
fn a_sign_in_without_an_invite_is_refused_and_creates_nothing() {
    let outsider = unique("mallory");
    let redirect_uri = loopback();
    let (_, challenge) = pkce();
    let landing = browse(
        &redirect_uri,
        &challenge,
        &unique("appstate"),
        &outsider,
        next_id(),
    );
    assert!(landing.location.starts_with(&redirect_uri));
    assert_eq!(landing.param("error").as_deref(), Some("access_denied"));
    assert!(landing.param("code").is_none());
    let members = http("GET", "/v1/members", Some(owner()), None).json();
    assert!(
        members["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["github_login"] != json!(outsider))
    );

    // A revoked invite is as good as none.
    let carol = unique("carol");
    let created = invite(owner(), &carol, None);
    assert_eq!(created.status, 201);
    let invite_id = created.json()["invite"]["id"].as_str().unwrap().to_owned();
    assert_eq!(
        http(
            "DELETE",
            &format!("/v1/invites/{invite_id}"),
            Some(owner()),
            None
        )
        .status,
        200
    );
    let landing = browse(
        &redirect_uri,
        &challenge,
        &unique("appstate"),
        &carol,
        next_id(),
    );
    assert_eq!(landing.param("error").as_deref(), Some("access_denied"));
}

#[test]
fn invites_are_by_github_login_and_never_above_the_inviters_role() {
    let viewer = session("viewer");
    let member = session("member");
    for token in [&viewer, &member] {
        assert_eq!(invite(token, &unique("x"), None).status, 403);
    }
    assert_eq!(invite("smt_nope", "someone", None).status, 401);
    assert_eq!(
        http(
            "POST",
            "/v1/invites",
            None,
            Some(&json!({ "github_login": "someone" }))
        )
        .status,
        401
    );

    let admin = session("admin");
    let (bob, erin) = (unique("bob"), unique("erin"));
    // The owner session minted the admin; an admin cannot invite an owner, but may invite an admin.
    assert_eq!(invite(&admin, &erin, Some("owner")).status, 403);
    assert_eq!(invite(&admin, "not a login", None).status, 400);
    assert_eq!(invite(&admin, &bob, Some("admin")).status, 201);
    assert_eq!(
        invite(&admin, &bob.to_ascii_uppercase(), None).status,
        409,
        "logins are case-insensitive"
    );

    // The invite is accepted by that GitHub account on its first sign-in and carries the role.
    let reply = desktop_sign_in(&bob.to_ascii_uppercase(), next_id());
    assert_eq!(reply.status, 201, "{}", reply.body);
    assert_eq!(reply.json()["role"], json!("admin"));
    assert_eq!(invite(&admin, &bob, None).status, 409, "now a member");
}

#[test]
fn redirect_uris_are_an_allowlist_and_never_an_open_redirect() {
    let (_, challenge) = pkce();
    let state = unique("appstate");
    for evil in [
        "https://evil.example/cb".to_owned(),
        "http://evil.example:8080/callback".to_owned(),
        "http://localhost:8080/callback".to_owned(),
        "https://127.0.0.1:8080/callback".to_owned(),
        "http://127.0.0.1:8080/steal".to_owned(),
        "http://127.0.0.1:8080@evil.example/callback".to_owned(),
        "http://127.0.0.1:8080/callback?next=https://evil.example".to_owned(),
        format!("http://{}/auth/github/callback/../x", hub_authority()),
        "http://other-hub.example/auth/github/callback".to_owned(),
        "myapp://callback".to_owned(),
    ] {
        let reply = http(
            "GET",
            &authorize_path(&evil, &challenge, &state),
            None,
            None,
        );
        assert_eq!(reply.status, 400, "{evil}: {}", reply.body);
        assert!(
            reply.header("location").is_none(),
            "{evil} must not redirect"
        );
    }
    // Only S256, a well-formed challenge and an app state.
    let ok = loopback();
    let base = authorize_path(&ok, &challenge, &state);
    for (label, path) in [
        ("plain", base.replace("S256", "plain")),
        ("short challenge", base.replace(&challenge, "abc")),
        ("no state", base.replace(&format!("&state={state}"), "")),
        (
            "no challenge",
            base.replace(&format!("&code_challenge={challenge}"), ""),
        ),
    ] {
        assert_eq!(http("GET", &path, None, None).status, 400, "{label}");
    }
    assert_eq!(http("GET", &base, None, None).status, 302);
}

#[test]
fn the_web_page_flow_returns_to_the_hubs_own_origin() {
    let login = unique("webby");
    assert_eq!(invite(owner(), &login, Some("viewer")).status, 201);
    let redirect_uri = format!("http://{}/auth/github/callback", hub_authority());
    let (verifier, challenge) = pkce();
    let state = unique("appstate");
    let landing = browse(&redirect_uri, &challenge, &state, &login, next_id());
    assert!(
        landing
            .location
            .starts_with(&format!("http://{}/#/auth/github?code=", hub_authority())),
        "{}",
        landing.location
    );
    let code = landing.param("code").expect("code");
    let reply = exchange(&code, &verifier, &redirect_uri);
    assert_eq!(token_of(&reply).len(), 68);
    assert_eq!(reply.json()["role"], json!("viewer"));
}

#[test]
fn the_state_is_single_use_and_a_made_up_state_goes_nowhere() {
    let login = unique("stately");
    assert_eq!(invite(owner(), &login, None).status, 201);
    let redirect_uri = loopback();
    let (_, challenge) = pkce();
    let landing = browse(
        &redirect_uri,
        &challenge,
        &unique("appstate"),
        &login,
        next_id(),
    );
    assert!(landing.param("code").is_some());
    // Replaying GitHub's redirect (same code, same state) is refused: 400 JSON, no redirect.
    let replay = http("GET", &landing.callback_path, None, None);
    assert_eq!(replay.status, 400, "{}", replay.body);
    assert!(replay.header("location").is_none());
    for path in [
        "/auth/github/callback?code=x&state=ost_unknown",
        "/auth/github/callback?code=x",
        "/auth/github/callback",
    ] {
        let reply = http("GET", path, None, None);
        assert_eq!(reply.status, 400, "{path}");
        assert!(reply.header("location").is_none(), "{path}");
    }
}

#[test]
fn a_code_is_single_use_and_needs_its_own_verifier_and_redirect() {
    let login = unique("pkce");
    let id = next_id();
    assert_eq!(invite(owner(), &login, None).status, 201);
    let redirect_uri = loopback();
    let (verifier, challenge) = pkce();
    let landing = browse(&redirect_uri, &challenge, &unique("appstate"), &login, id);
    let code = landing.param("code").expect("code");

    // An attacker who stole the code but not the verifier gets nothing, and burns the code.
    let (other_verifier, _) = pkce();
    let miss = exchange(&code, &other_verifier, &redirect_uri);
    assert_eq!(miss.status, 400);
    assert_eq!(miss.json()["error"], json!("invalid_grant"));
    assert_eq!(
        exchange(&code, &verifier, &redirect_uri).status,
        400,
        "the code is burned"
    );

    // The right verifier alone is not enough with a different redirect URI.
    let landing = browse(&redirect_uri, &challenge, &unique("appstate"), &login, id);
    let code = landing.param("code").expect("code");
    assert_eq!(
        exchange(&code, &verifier, "http://127.0.0.1:1/callback").status,
        400
    );

    // Works once, then never again.
    let landing = browse(&redirect_uri, &challenge, &unique("appstate"), &login, id);
    let code = landing.param("code").expect("code");
    assert_eq!(exchange(&code, &verifier, &redirect_uri).status, 201);
    assert_eq!(exchange(&code, &verifier, &redirect_uri).status, 400);
    for bad in [
        json!({}),
        json!({ "code": "oac_x" }),
        json!({ "code": "oac_x", "code_verifier": "short", "redirect_uri": "x" }),
    ] {
        assert!(matches!(
            http("POST", "/v1/auth/github/token", None, Some(&bad)).status,
            400
        ));
    }
}

#[test]
fn the_same_github_account_keeps_one_hub_user_across_a_login_rename() {
    let login = unique("rename");
    let id = next_id();
    assert_eq!(invite(owner(), &login, None).status, 201);
    let first = desktop_sign_in(&login, id).json();
    let renamed = unique("renamed");
    // Same GitHub id under a new login: the same hub user, no invite needed again.
    let second = desktop_sign_in(&renamed, id).json();
    assert_eq!(first["user"]["id"], second["user"]["id"]);
    // A different account that takes over the old login is not that user and has no invite.
    let reply = {
        let (_, challenge) = pkce();
        browse(
            &loopback(),
            &challenge,
            &unique("appstate"),
            &login,
            next_id(),
        )
    };
    assert_eq!(reply.param("error").as_deref(), Some("access_denied"));
}

#[test]
fn membership_is_required_for_org_routes_and_removal_ends_the_session() {
    let login = unique("member");
    assert_eq!(invite(owner(), &login, None).status, 201);
    let reply = desktop_sign_in(&login, next_id());
    let token = token_of(&reply);
    let user_id = reply.json()["user"]["id"].as_str().unwrap().to_owned();

    // Without a session: 401 on the org routes. With a member session: allowed, and another
    // org's or an unknown thread is 404 (the existing gate).
    for path in ["/v1/members", "/v1/threads/t_none"] {
        assert_eq!(http("GET", path, None, None).status, 401, "{path}");
    }
    assert_eq!(http("GET", "/v1/members", Some(&token), None).status, 200);
    assert_eq!(
        http("GET", "/v1/threads/t_none", Some(&token), None).status,
        404
    );
    // A member cannot remove anyone.
    assert_eq!(
        http(
            "DELETE",
            &format!("/v1/members/{user_id}"),
            Some(&token),
            None
        )
        .status,
        403
    );

    // The owner removes them: the very next request with the same token is 401.
    let removed = http(
        "DELETE",
        &format!("/v1/members/{user_id}"),
        Some(owner()),
        None,
    );
    assert_eq!(removed.status, 200, "{}", removed.body);
    for path in ["/v1/members", "/v1/threads/t_none"] {
        assert_eq!(http("GET", path, Some(&token), None).status, 401, "{path}");
    }
    // Signing in again does not bring them back: they are a known GitHub account with no
    // membership and no open invite.
    let (_, challenge) = pkce();
    let landing = browse(
        &loopback(),
        &challenge,
        &unique("appstate"),
        &login,
        next_id(),
    );
    assert_eq!(landing.param("error").as_deref(), Some("access_denied"));
}

/// M2-003e: the unauthenticated start is limited per `CF-Connecting-IP`; other IPs and the rest of
/// the suite (which sends no such header) are not affected.
/// The bootstrap owner (the one `owner()` made) is the only owner in the org: no test issues one.
#[test]
fn the_bootstrap_owner_signs_in_with_the_configured_github_login_and_no_other_login_can() {
    let listed = http("GET", "/v1/members", Some(owner()), None).json();
    let owner_id = listed["members"]
        .as_array()
        .expect("members")
        .iter()
        .find(|m| m["role"] == json!("owner"))
        .expect("the bootstrap owner")["user_id"]
        .clone();
    let login = env("BOOTSTRAP_OWNER_GITHUB_LOGIN");

    // Another GitHub account, with no invite, is refused and linked to nothing.
    let (_, challenge) = pkce();
    let landing = browse(
        &loopback(),
        &challenge,
        &unique("appstate"),
        &unique("mallory"),
        next_id(),
    );
    assert_eq!(landing.param("error").as_deref(), Some("access_denied"));

    // The configured login signs in as the owner, whatever the age of the owner's account.
    // One GitHub account is one numeric id: the sign-ins below reuse it.
    let github_id = next_id();
    let reply = desktop_sign_in(&login, github_id);
    token_of(&reply);
    let body = reply.json();
    assert_eq!(body["role"], json!("owner"), "{body}");
    assert_eq!(body["user"]["id"], owner_id, "{body}");

    // Again, the same account (found by its GitHub id now).
    let again = desktop_sign_in(&login, github_id);
    token_of(&again);
    assert_eq!(again.json()["user"]["id"], owner_id);

    // A different GitHub account with the same login is not the owner: the link is used up.
    let (_, challenge) = pkce();
    let stranger = browse(
        &loopback(),
        &challenge,
        &unique("appstate"),
        &login,
        next_id(),
    );
    assert_eq!(stranger.param("error").as_deref(), Some("access_denied"));

    // The link is audited, and says it came from the bootstrap login.
    assert!(
        audit_actions()
            .iter()
            .any(|(action, row)| action == "auth.github.link"
                && row.contains("bootstrap_owner_login"))
    );
}

#[test]
fn authorize_is_rate_limited_per_connecting_ip() {
    let (_, challenge) = pkce();
    let path = authorize_path(&loopback(), &challenge, &unique("ratelimit"));
    let start = |ip: &str| {
        http_with(
            &hub_authority(),
            "GET",
            &path,
            None,
            None,
            &[("CF-Connecting-IP", ip)],
        )
    };
    // A documentation-range address, unique per run so a rerun inside the same minute does not
    // start already limited.
    let ip = format!("198.51.100.{}", next_id() % 250 + 1);
    let statuses: Vec<u16> = (0..12).map(|_| start(&ip).status).collect();
    assert_eq!(&statuses[..10], &[302; 10], "the first ten are allowed");
    assert_eq!(&statuses[10..], &[429, 429], "then the limit holds");
    let limited = start(&ip);
    assert_eq!(limited.json()["error"], json!("rate_limited"));
    let wait: u64 = limited
        .header("retry-after")
        .and_then(|v| v.parse().ok())
        .expect("Retry-After seconds");
    assert!((1..=60).contains(&wait), "retry after {wait}");
    // Another address is untouched.
    assert_eq!(start("203.0.113.250").status, 302);
}
