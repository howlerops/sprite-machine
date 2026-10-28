//! Native tests of GitHub sign-in (M2-003b) over an in-memory SQLite that runs the real
//! migrations, with the GitHub reply injected (`GithubUser`). The live test with a mock GitHub
//! over HTTP is `tests/oauth.rs`. [`Fake`] and [`seed_org`] are shared with the members tests.

use std::cell::RefCell;
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::github::{
    CODE_TTL_MS, GithubConfig, GithubUser, INVITE_TTL_MS, Pending, RedirectKind, STATE_TTL_MS,
    authorize, begin_callback, classify_redirect, error_location, finish_callback, parse_query,
    parse_token_response, parse_user, purge, redeem, s256_challenge,
};
use super::{Principal, authenticate};
use crate::db::{Db, DbError, MIGRATIONS, Param, Role, Row};
use crate::devices::limits;
use crate::threads::ApiError;

pub(crate) const T0: i64 = 1_800_000_000_000;
pub(crate) const HUB: &str = "https://hub.example";
const LOOPBACK: &str = "http://127.0.0.1:53124/callback";
const VERIFIER: &str = "dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk";
const APP_STATE: &str = "app-state-0123456789";

pub(crate) struct Fake {
    conn: RefCell<rusqlite::Connection>,
}

fn backend(error: rusqlite::Error) -> DbError {
    DbError::Backend(error.to_string())
}

fn bind(params: &[Param]) -> Vec<SqlValue> {
    params
        .iter()
        .map(|param| match param {
            Param::Null => SqlValue::Null,
            Param::Int(value) => SqlValue::Integer(*value),
            Param::Real(value) => SqlValue::Real(*value),
            Param::Text(text) => SqlValue::Text(text.clone()),
        })
        .collect()
}

fn run_query(
    conn: &rusqlite::Connection,
    sql: &str,
    params: &[Param],
) -> Result<Vec<Row>, DbError> {
    let mut statement = conn.prepare(sql).map_err(backend)?;
    let names: Vec<String> = statement.column_names().map(|n| n.to_string()).collect();
    let mut rows = statement
        .query(params_from_iter(bind(params)))
        .map_err(backend)?;
    let mut out = Vec::new();
    while let Some(row) = rows.next().map_err(backend)? {
        let mut object = Row::new();
        for (index, name) in names.iter().enumerate() {
            let value: SqlValue = row.get(index).map_err(backend)?;
            object.insert(
                name.clone(),
                match value {
                    SqlValue::Null | SqlValue::Blob(_) => Value::Null,
                    SqlValue::Integer(v) => json!(v),
                    SqlValue::Real(v) => json!(v),
                    SqlValue::Text(t) => Value::String(t),
                },
            );
        }
        out.push(object);
    }
    Ok(out)
}

impl Fake {
    pub(crate) fn new() -> Self {
        let conn = rusqlite::Connection::open_in_memory().expect("open sqlite");
        conn.pragma_update(None, "foreign_keys", true)
            .expect("foreign keys");
        for (name, sql) in MIGRATIONS {
            conn.execute_batch(sql)
                .unwrap_or_else(|error| panic!("migration {name}: {error}"));
        }
        Self {
            conn: RefCell::new(conn),
        }
    }

    pub(crate) fn rows(&self, sql: &str) -> Vec<Row> {
        run_query(&self.conn.borrow(), sql, &[]).expect("query")
    }

    pub(crate) fn exec(&self, sql: &str) {
        self.conn.borrow().execute_batch(sql).expect("exec");
    }

    pub(crate) fn count(&self, sql: &str) -> i64 {
        self.rows(sql)[0]["n"].as_i64().expect("count")
    }
}

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        let conn = self.conn.borrow();
        ready(
            conn.prepare(sql)
                .and_then(|mut s| s.execute(params_from_iter(bind(params))))
                .map(|n| n as u64)
                .map_err(backend),
        )
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        ready(run_query(&self.conn.borrow(), sql, params))
    }
}

/// Drives a future that only ever returns `Ready` (the fake's).
pub(crate) fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = pin!(future);
    let mut context = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut context) {
        Poll::Ready(output) => output,
        Poll::Pending => panic!("the in-memory fake never returns Pending"),
    }
}

/// An org with an owner (user, device and membership); returns the owner as a principal.
pub(crate) fn seed_org(db: &Fake, org_id: &str, owner_id: &str) -> Principal {
    db.exec(&format!(
        "INSERT INTO orgs (id, slug, name) VALUES ('{org_id}', '{org_id}', 'Org {org_id}');
         INSERT INTO users (id, display_name) VALUES ('{owner_id}', 'Owner {owner_id}');
         INSERT INTO members (org_id, user_id, role) VALUES ('{org_id}', '{owner_id}', 'owner');
         INSERT INTO devices (id, org_id, user_id, name) VALUES ('dev_{owner_id}', '{org_id}', '{owner_id}', 'cli');"
    ));
    Principal {
        user_id: owner_id.to_owned(),
        device_id: format!("dev_{owner_id}"),
        org_id: org_id.to_owned(),
        role: Role::Owner,
        display_name: String::new(),
        session_id: None,
    }
}

fn config() -> GithubConfig {
    GithubConfig {
        client_id: "test-client-id".to_owned(),
        api_base: Some("http://mock.invalid".to_owned()),
        hub_origin: HUB.to_owned(),
        bootstrap_login: None,
    }
}

/// `config()` with `BOOTSTRAP_OWNER_GITHUB_LOGIN` set to `login` (lowercase, as the routes make it).
fn config_with_bootstrap(login: &str) -> GithubConfig {
    GithubConfig {
        bootstrap_login: Some(login.to_owned()),
        ..config()
    }
}

/// Makes `usr_boot` the bootstrap owner of `org_id`: the `auth.bootstrap` audit row names it.
fn audit_bootstrap(db: &Fake, org_id: &str, owner_id: &str) {
    db.exec(&format!(
        "INSERT INTO audit_log (id, org_id, action, detail_json) \
         VALUES ('aud_boot', '{org_id}', 'auth.bootstrap', '{{\"user_id\":\"{owner_id}\"}}')"
    ));
}

fn query(redirect_uri: &str) -> Vec<(String, String)> {
    vec![
        ("redirect_uri".to_owned(), redirect_uri.to_owned()),
        ("code_challenge".to_owned(), s256_challenge(VERIFIER)),
        ("code_challenge_method".to_owned(), "S256".to_owned()),
        ("state".to_owned(), APP_STATE.to_owned()),
    ]
}

fn with(
    mut params: Vec<(String, String)>,
    key: &str,
    value: Option<&str>,
) -> Vec<(String, String)> {
    params.retain(|(name, _)| name != key);
    if let Some(value) = value {
        params.push((key.to_owned(), value.to_owned()));
    }
    params
}

fn start(db: &Fake, params: &[(String, String)], now: i64) -> Result<String, ApiError> {
    block_on(authorize(db, &config(), params, None, now))
}

/// The `state` GitHub would send back, read from the redirect to GitHub.
fn state_of(github_url: &str) -> String {
    let (_, q) = github_url.split_once('?').expect("query");
    parse_query(q)
        .into_iter()
        .find(|(name, _)| name == "state")
        .expect("state")
        .1
}

fn consume(db: &Fake, github_url: &str, now: i64) -> Option<Pending> {
    block_on(begin_callback(db, &state_of(github_url), now)).expect("begin")
}

fn alice(id: i64, login: &str) -> GithubUser {
    GithubUser {
        id,
        login: login.to_owned(),
        name: Some("Alice A".to_owned()),
        email: None,
    }
}

fn invite(db: &Fake, id: &str, org: &str, login: &str, role: &str, now: i64) {
    db.exec(&format!(
        "INSERT INTO org_invites (id, org_id, github_login, role, created_at_ms, expires_at_ms) \
         VALUES ('{id}', '{org}', '{login}', '{role}', {now}, {})",
        now + INVITE_TTL_MS
    ));
}

/// Runs authorize, the callback and the finish for `user`; returns the app redirect.
fn sign_in(db: &Fake, user: &GithubUser, redirect_uri: &str, now: i64) -> String {
    let url = start(db, &query(redirect_uri), now).expect("authorize");
    let pending = consume(db, &url, now).expect("pending");
    block_on(finish_callback(db, &config(), &pending, user, now)).expect("finish")
}

fn param_of(location: &str, key: &str) -> Option<String> {
    let (_, q) = location.split_once('?')?;
    parse_query(q)
        .into_iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v)
}

fn token(
    db: &Fake,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
    now: i64,
) -> Result<Value, ApiError> {
    let body = json!({ "code": code, "code_verifier": verifier, "redirect_uri": redirect_uri });
    block_on(redeem(db, &config(), &body, now, "req_t"))
}

// ---- redirect allowlist ---------------------------------------------------------------------

#[test]
fn only_loopback_ips_and_the_hubs_own_callback_are_allowed_redirects() {
    for ok in [
        "http://127.0.0.1:53124/callback",
        "http://127.0.0.1:1/callback",
        "http://127.0.0.1:65535/callback",
        "http://[::1]:8080/callback",
    ] {
        assert_eq!(
            classify_redirect(ok, HUB),
            Some(RedirectKind::Loopback),
            "{ok}"
        );
    }
    assert_eq!(
        classify_redirect("https://hub.example/auth/github/callback", HUB),
        Some(RedirectKind::Web)
    );
    for bad in [
        "http://localhost:53124/callback",
        "https://127.0.0.1:53124/callback",
        "http://127.0.0.1/callback",
        "http://127.0.0.1:0/callback",
        "http://127.0.0.1:65536/callback",
        "http://127.0.0.1:053124/callback",
        "http://127.0.0.1:5x/callback",
        "http://127.0.0.1:53124/other",
        "http://127.0.0.1:53124/callback/",
        "http://127.0.0.1:53124/callback?x=1",
        "http://127.0.0.1:53124/callback#frag",
        "http://127.0.0.1:53124@evil.example/callback",
        "http://127.0.0.1.evil.example:53124/callback",
        "http://evil.example:53124/callback",
        "http://127.0.0.2:53124/callback",
        "http://127.0.0.1:53124/callback evil",
        "http://127.0.0.1:53124\\@evil.example/callback",
        "https://evil.example/auth/github/callback",
        "https://hub.example/auth/github/callback?next=//evil.example",
        "https://hub.example/auth/github/callback/",
        "https://hub.example.evil.example/auth/github/callback",
        "http://hub.example/auth/github/callback",
        "myapp://callback",
        "",
    ] {
        assert_eq!(classify_redirect(bad, HUB), None, "{bad}");
    }
}

#[test]
fn authorize_refuses_a_bad_redirect_a_non_s256_method_and_malformed_parameters() {
    let db = Fake::new();
    let good = query(LOOPBACK);
    for (label, params) in [
        (
            "evil redirect",
            with(
                good.clone(),
                "redirect_uri",
                Some("https://evil.example/cb"),
            ),
        ),
        ("no redirect", with(good.clone(), "redirect_uri", None)),
        (
            "plain",
            with(good.clone(), "code_challenge_method", Some("plain")),
        ),
        (
            "no method",
            with(good.clone(), "code_challenge_method", None),
        ),
        ("no challenge", with(good.clone(), "code_challenge", None)),
        (
            "short challenge",
            with(good.clone(), "code_challenge", Some("abc")),
        ),
        ("no state", with(good.clone(), "state", None)),
        ("short state", with(good.clone(), "state", Some("abc"))),
        (
            "state with a space",
            with(good.clone(), "state", Some("aaaaaaaa bbbbbbbb")),
        ),
        ("bad org", with(good.clone(), "org_id", Some("a b"))),
    ] {
        let error = start(&db, &params, T0).expect_err(label);
        assert_eq!(
            (error.status, error.code),
            (400, "invalid_request"),
            "{label}"
        );
    }
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM oauth_requests"), 0);
    assert!(start(&db, &good, T0).is_ok());
}

// ---- state and the GitHub leg ---------------------------------------------------------------

#[test]
fn the_state_is_hashed_single_use_short_lived_and_bound_to_the_verifier() {
    let db = Fake::new();
    let url = start(&db, &query(LOOPBACK), T0).expect("authorize");
    assert!(url.starts_with("http://mock.invalid/login/oauth/authorize?client_id=test-client-id&"));
    assert!(url.contains("redirect_uri=https%3A%2F%2Fhub.example%2Fauth%2Fgithub%2Fcallback"));
    assert!(url.contains("code_challenge_method=S256"));
    let state = state_of(&url);
    // Only the hash is stored.
    let stored = db.rows("SELECT state_hash, github_verifier FROM oauth_requests");
    assert_eq!(stored.len(), 1);
    assert_ne!(stored[0]["state_hash"], json!(state));
    assert_eq!(stored[0]["state_hash"].as_str().unwrap().len(), 64);
    // The challenge sent to GitHub is the S256 of the verifier kept for the exchange.
    let verifier = stored[0]["github_verifier"].as_str().unwrap();
    assert!(url.contains(&format!("code_challenge={}", s256_challenge(verifier))));

    let pending = consume(&db, &url, T0 + 1_000).expect("first use");
    assert_eq!(pending.github_verifier, verifier);
    assert_eq!(pending.redirect_uri, LOOPBACK);
    assert_eq!(pending.app_state, APP_STATE);
    assert!(consume(&db, &url, T0 + 2_000).is_none(), "single use");

    let url = start(&db, &query(LOOPBACK), T0).expect("authorize");
    assert!(consume(&db, &url, T0 + STATE_TTL_MS).is_none(), "expired");
    for bad in ["", "ost_unknown", "no-prefix", &"ost_".repeat(100)] {
        assert!(
            block_on(begin_callback(&db, bad, T0))
                .expect("begin")
                .is_none()
        );
    }
}

#[test]
fn two_authorizes_get_distinct_states_and_verifiers() {
    let db = Fake::new();
    let a = start(&db, &query(LOOPBACK), T0).expect("a");
    let b = start(&db, &query(LOOPBACK), T0).expect("b");
    assert_ne!(state_of(&a), state_of(&b));
    let rows = db.rows("SELECT DISTINCT github_verifier FROM oauth_requests");
    assert_eq!(rows.len(), 2);
}

// ---- sign-in, invites, and the token endpoint -----------------------------------------------

#[test]
fn an_invited_account_signs_in_becomes_a_member_and_trades_the_code_once() {
    let db = Fake::new();
    let owner = seed_org(&db, "org_a", "usr_owner");
    invite(&db, "inv_1", "org_a", "alice", "member", T0);

    let location = sign_in(&db, &alice(1001, "Alice"), LOOPBACK, T0 + 5_000);
    assert!(
        location.starts_with("http://127.0.0.1:53124/callback?"),
        "{location}"
    );
    assert_eq!(param_of(&location, "state").as_deref(), Some(APP_STATE));
    let code = param_of(&location, "code").expect("code");
    assert!(code.starts_with("oac_"));
    // Mapped by GitHub id; the login is stored as GitHub spells it.
    let users = db
        .rows("SELECT id, github_id, github_login, display_name FROM users WHERE github_id = 1001");
    assert_eq!(users.len(), 1);
    assert_eq!(users[0]["github_login"], json!("Alice"));
    assert_eq!(users[0]["display_name"], json!("Alice A"));
    let members =
        db.rows("SELECT role FROM members WHERE org_id = 'org_a' AND user_id <> 'usr_owner'");
    assert_eq!(members[0]["role"], json!("member"));
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM org_invites WHERE accepted_at_ms IS NOT NULL"),
        1
    );
    // The hub code is stored hashed.
    assert_eq!(
        db.count(&format!(
            "SELECT COUNT(*) AS n FROM oauth_codes WHERE code_hash = '{code}'"
        )),
        0
    );

    let granted = token(&db, &code, VERIFIER, LOOPBACK, T0 + 6_000).expect("token");
    let session = granted["token"].as_str().expect("token");
    assert!(session.starts_with("smt_"));
    assert_eq!(granted["role"], json!("member"));
    assert_eq!(granted["org"]["id"], json!("org_a"));
    let principal =
        block_on(authenticate(&db, Some(&format!("Bearer {session}")), false)).expect("session");
    assert_eq!(principal.org_id, "org_a");
    assert_eq!(principal.role, Role::Member);
    assert_ne!(principal.user_id, owner.user_id);
    // Audited without the code, the verifier or the token.
    let audit = db.rows("SELECT action, detail_json FROM audit_log ORDER BY seq");
    let actions: Vec<_> = audit
        .iter()
        .map(|r| r["action"].as_str().unwrap())
        .collect();
    assert_eq!(actions, ["auth.github.signin", "member.invite.accept"]);
    let all: String = audit
        .iter()
        .map(|r| r["detail_json"].as_str().unwrap())
        .collect();
    assert!(!all.contains(&code) && !all.contains(session) && !all.contains(VERIFIER));

    // Single use.
    let replay = token(&db, &code, VERIFIER, LOOPBACK, T0 + 7_000).expect_err("replay");
    assert_eq!((replay.status, replay.code), (400, "invalid_grant"));
}

#[test]
fn the_token_endpoint_needs_the_verifier_the_redirect_and_a_fresh_code_and_burns_it_on_a_miss() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    invite(&db, "inv_1", "org_a", "alice", "viewer", T0);
    let code_for = |db: &Fake, now| {
        param_of(&sign_in(db, &alice(1001, "alice"), LOOPBACK, now), "code").expect("code")
    };

    let code = code_for(&db, T0);
    let wrong = "A".repeat(43);
    let miss = token(&db, &code, &wrong, LOOPBACK, T0 + 1).expect_err("wrong verifier");
    assert_eq!((miss.status, miss.code), (400, "invalid_grant"));
    assert!(
        token(&db, &code, VERIFIER, LOOPBACK, T0 + 2).is_err(),
        "a miss burns the code"
    );

    let code = code_for(&db, T0);
    assert!(
        token(
            &db,
            &code,
            VERIFIER,
            "http://127.0.0.1:1111/callback",
            T0 + 1
        )
        .is_err()
    );
    assert!(token(&db, &code, VERIFIER, LOOPBACK, T0 + 2).is_err());

    let code = code_for(&db, T0);
    assert!(
        token(&db, &code, VERIFIER, LOOPBACK, T0 + CODE_TTL_MS).is_err(),
        "expired"
    );

    let code = code_for(&db, T0);
    assert!(token(&db, &code, "short", LOOPBACK, T0 + 1).is_err());
    assert!(token(&db, "oac_unknown", VERIFIER, LOOPBACK, T0 + 1).is_err());
    let granted =
        token(&db, &code, VERIFIER, LOOPBACK, T0 + 3).expect("untouched code still works");
    assert_eq!(granted["role"], json!("viewer"));
}

#[test]
fn an_account_is_found_by_its_github_id_not_its_login() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    invite(&db, "inv_1", "org_a", "alice", "member", T0);
    let first = sign_in(&db, &alice(1001, "alice"), LOOPBACK, T0);
    assert!(param_of(&first, "code").is_some());
    let user_id = db.rows("SELECT id FROM users WHERE github_id = 1001")[0]["id"].clone();

    // Alice renames to alice2: same id, same hub user, login refreshed.
    let renamed = sign_in(&db, &alice(1001, "alice2"), LOOPBACK, T0 + 1);
    assert!(param_of(&renamed, "code").is_some());
    let rows = db.rows("SELECT id, github_login FROM users WHERE github_id = 1001");
    assert_eq!((rows.len(), &rows[0]["id"]), (1, &user_id));
    assert_eq!(rows[0]["github_login"], json!("alice2"));

    // A different account (id 2002) now owns the login "alice". It is not Alice: no invite for it
    // under its own id, and the old hub user is not handed over by name.
    let other = sign_in(&db, &alice(2002, "alice"), LOOPBACK, T0 + 2);
    assert_eq!(param_of(&other, "error").as_deref(), Some("access_denied"));
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM users WHERE github_id = 2002"),
        0
    );
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM users"), 2);
}

#[test]
fn an_account_with_no_hub_user_and_no_open_invite_gets_nothing() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    let location = sign_in(&db, &alice(3003, "mallory"), LOOPBACK, T0);
    assert!(param_of(&location, "code").is_none());
    assert_eq!(
        param_of(&location, "error").as_deref(),
        Some("access_denied")
    );
    assert_eq!(param_of(&location, "state").as_deref(), Some(APP_STATE));
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM users"), 1);
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM oauth_codes"), 0);

    // Revoked, expired and other-login invites do not count.
    invite(&db, "inv_r", "org_a", "mallory", "member", T0);
    db.exec("UPDATE org_invites SET revoked_at_ms = 1 WHERE id = 'inv_r'");
    invite(
        &db,
        "inv_e",
        "org_a",
        "carol",
        "member",
        T0 - 2 * INVITE_TTL_MS,
    );
    invite(&db, "inv_o", "org_a", "someone-else", "member", T0);
    for (id, login) in [(3003, "mallory"), (4004, "carol")] {
        let location = sign_in(&db, &alice(id, login), LOOPBACK, T0);
        assert_eq!(
            param_of(&location, "error").as_deref(),
            Some("access_denied"),
            "{login}"
        );
    }
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM users"), 1);
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM members"), 1);
}

#[test]
fn a_session_is_only_for_an_org_the_account_belongs_to() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_a");
    seed_org(&db, "org_b", "usr_b");
    invite(&db, "inv_1", "org_a", "alice", "admin", T0);
    // Asking for org_b, where alice is not a member, is denied (no fallback to org_a).
    let url = start(&db, &with(query(LOOPBACK), "org_id", Some("org_b")), T0).expect("authorize");
    let pending = consume(&db, &url, T0).expect("pending");
    let location = block_on(finish_callback(
        &db,
        &config(),
        &pending,
        &alice(1001, "alice"),
        T0,
    ))
    .expect("finish");
    assert_eq!(
        param_of(&location, "error").as_deref(),
        Some("access_denied")
    );
    // Without an org, or with the right one, the session is for org_a.
    for org in [None, Some("org_a")] {
        let params = with(query(LOOPBACK), "org_id", org);
        let url = start(&db, &params, T0).expect("authorize");
        let pending = consume(&db, &url, T0).expect("pending");
        let location = block_on(finish_callback(
            &db,
            &config(),
            &pending,
            &alice(1001, "alice"),
            T0,
        ))
        .expect("finish");
        let code = param_of(&location, "code").expect("code");
        let granted = token(&db, &code, VERIFIER, LOOPBACK, T0).expect("token");
        assert_eq!(
            (granted["org"]["id"].as_str(), granted["role"].as_str()),
            (Some("org_a"), Some("admin"))
        );
    }
}

#[test]
fn a_member_removed_after_the_code_was_issued_gets_no_session() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    invite(&db, "inv_1", "org_a", "alice", "member", T0);
    let code = param_of(&sign_in(&db, &alice(1001, "alice"), LOOPBACK, T0), "code").expect("code");
    db.exec("DELETE FROM members WHERE user_id IN (SELECT id FROM users WHERE github_id = 1001)");
    let error = token(&db, &code, VERIFIER, LOOPBACK, T0 + 1).expect_err("no longer a member");
    assert_eq!(error.code, "invalid_grant");
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM sessions"), 0);
}

#[test]
fn the_web_page_gets_the_result_in_the_fragment_of_the_hubs_own_origin() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    invite(&db, "inv_1", "org_a", "alice", "member", T0);
    let web = "https://hub.example/auth/github/callback";
    let location = sign_in(&db, &alice(1001, "alice"), web, T0);
    assert!(
        location.starts_with("https://hub.example/#/auth/github?code=oac_"),
        "{location}"
    );
    let code = location
        .split("code=")
        .nth(1)
        .unwrap()
        .split('&')
        .next()
        .unwrap();
    let granted = token(&db, code, VERIFIER, web, T0 + 1).expect("token");
    let device = db.rows("SELECT name FROM devices WHERE org_id = 'org_a' AND name = 'web'");
    assert_eq!(device.len(), 1);
    assert!(granted["token"].as_str().unwrap().starts_with("smt_"));

    let pending = Pending {
        github_verifier: String::new(),
        redirect_uri: web.to_owned(),
        challenge: String::new(),
        app_state: APP_STATE.to_owned(),
        org_id: None,
    };
    assert_eq!(
        error_location(&pending, HUB, "access_denied"),
        format!("https://hub.example/#/auth/github?error=access_denied&state={APP_STATE}")
    );
}

#[test]
fn an_existing_user_with_the_login_and_no_github_id_is_linked_once() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    db.exec("UPDATE users SET github_login = 'Boss' WHERE id = 'usr_owner'");
    age_user(&db, "usr_owner", T0, DAY_MS);
    let location = sign_in(&db, &alice(7007, "boss"), LOOPBACK, T0);
    let code = param_of(&location, "code").expect("code");
    let granted = token(&db, &code, VERIFIER, LOOPBACK, T0).expect("token");
    assert_eq!(granted["role"], json!("owner"));
    assert_eq!(granted["user"]["id"], json!("usr_owner"));
    assert_eq!(
        db.rows("SELECT github_id FROM users WHERE id = 'usr_owner'")[0]["github_id"],
        json!(7007)
    );
    // Another account that merely has the same login cannot take the user over now.
    let other = sign_in(&db, &alice(8008, "boss"), LOOPBACK, T0);
    assert_eq!(param_of(&other, "error").as_deref(), Some("access_denied"));
}

// ---- sign-in hardening (M2-003e) -------------------------------------------------------------

fn start_from(db: &Fake, ip: Option<&str>, now: i64) -> Result<String, ApiError> {
    block_on(authorize(db, &config(), &query(LOOPBACK), ip, now))
}

#[test]
fn authorize_is_limited_per_connecting_ip() {
    let db = Fake::new();
    for n in 0..limits::AUTHORIZE_PER_IP_PER_MINUTE {
        start_from(&db, Some("203.0.113.7"), T0 + n).expect("under the limit");
    }
    let limited = start_from(&db, Some("203.0.113.7"), T0 + 1000).expect_err("over the limit");
    assert_eq!(limited.status, 429);
    assert_eq!(limited.code, "rate_limited");
    assert!(limits::retry_after(&limited).is_some_and(|s| (1..=60).contains(&s)));
    // A refused start stores nothing.
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM oauth_requests"),
        limits::AUTHORIZE_PER_IP_PER_MINUTE
    );
    // Another IP, and a request with no IP (local dev), are not affected.
    start_from(&db, Some("203.0.113.8"), T0 + 1000).expect("other ip");
    start_from(&db, None, T0 + 1000).expect("no ip");
    // The window slides: a minute later the first IP may start again.
    start_from(&db, Some("203.0.113.7"), T0 + 61_000).expect("window passed");
}

#[test]
fn the_hourly_purge_drops_expired_requests_codes_and_invites_only() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_owner");
    // Two authorize requests, one expired by the clock below (state TTL 10 minutes).
    start_from(&db, None, T0).expect("old");
    start_from(&db, None, T0 + 9 * 60_000).expect("fresh");
    // One hub code per age.
    for (id, hash_char, created) in [("oac_old", "a", T0), ("oac_new", "b", T0 + 10 * 60_000)] {
        db.exec(&format!(
            "INSERT INTO oauth_codes (id, code_hash, user_id, org_id, redirect_uri, challenge, \
             created_at_ms, expires_at_ms) VALUES ('{id}', '{}', 'usr_owner', 'org_a', 'x', '{}', {created}, {})",
            hash_char.repeat(64),
            "c".repeat(43),
            created + CODE_TTL_MS
        ));
    }
    invite(
        &db,
        "inv_old",
        "org_a",
        "olduser",
        "member",
        T0 - INVITE_TTL_MS - 1000,
    );
    invite(
        &db,
        "inv_new",
        "org_a",
        "newuser",
        "member",
        T0 + 20 * 60_000,
    );

    let now = T0 + 11 * 60_000;
    let deleted = block_on(purge(&db, now)).expect("purge");
    assert_eq!(deleted, (1, 1, 1));
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM oauth_requests"), 1);
    let codes = db.rows("SELECT id FROM oauth_codes");
    assert_eq!((codes.len(), &codes[0]["id"]), (1, &json!("oac_new")));
    let invites = db.rows("SELECT id FROM org_invites");
    assert_eq!((invites.len(), &invites[0]["id"]), (1, &json!("inv_new")));
    // Nothing left to drop on the next pass.
    assert_eq!(block_on(purge(&db, now)).expect("again"), (0, 0, 0));
}

/// Backdates a user's `created_at` to `age_ms` before `now`.
fn age_user(db: &Fake, user_id: &str, now: i64, age_ms: i64) {
    db.exec(&format!(
        "UPDATE users SET created_at = strftime('%Y-%m-%dT%H:%M:%fZ', {}, 'unixepoch') WHERE id = '{user_id}'",
        (now - age_ms) / 1000
    ));
}

const DAY_MS: i64 = 24 * 60 * 60 * 1000;

#[test]
fn a_pre_created_user_is_linked_by_login_only_in_the_first_seven_days_and_the_link_is_audited() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_young");
    db.exec(
        "UPDATE users SET github_login = 'Young' WHERE id = 'usr_young';
         INSERT INTO users (id, display_name, github_login) VALUES ('usr_old', 'Old', 'Old');
         INSERT INTO members (org_id, user_id, role) VALUES ('org_a', 'usr_old', 'admin');",
    );
    age_user(&db, "usr_young", T0, 6 * DAY_MS);
    age_user(&db, "usr_old", T0, 8 * DAY_MS);

    // Younger than 7 days: linked, and the redeem audits it.
    let location = sign_in(&db, &alice(5001, "young"), LOOPBACK, T0);
    let code = param_of(&location, "code").expect("young is linked");
    let granted = token(&db, &code, VERIFIER, LOOPBACK, T0).expect("token");
    assert_eq!(granted["user"]["id"], json!("usr_young"));
    assert_eq!(
        db.rows("SELECT github_id FROM users WHERE id = 'usr_young'")[0]["github_id"],
        json!(5001)
    );
    let audited = db.rows("SELECT target_id FROM audit_log WHERE action = 'auth.github.link'");
    assert_eq!(audited.len(), 1);
    assert_eq!(audited[0]["target_id"], json!("usr_young"));

    // Older than 7 days: the login alone is not enough. Refused like an unknown account, and the
    // old user stays unlinked.
    let refused = sign_in(&db, &alice(5002, "old"), LOOPBACK, T0);
    assert_eq!(
        param_of(&refused, "error").as_deref(),
        Some("access_denied")
    );
    assert!(db.rows("SELECT github_id FROM users WHERE id = 'usr_old'")[0]["github_id"].is_null());
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM audit_log WHERE action = 'auth.github.link'"),
        1
    );

    // A fresh invite is the way in: a new user is made, and the stale user loses the login.
    invite(&db, "inv_old", "org_a", "old", "member", T0);
    let again = sign_in(&db, &alice(5002, "old"), LOOPBACK, T0 + 1);
    assert!(param_of(&again, "code").is_some());
    let fresh = db.rows("SELECT id FROM users WHERE github_id = 5002");
    assert_eq!(fresh.len(), 1);
    assert_ne!(fresh[0]["id"], json!("usr_old"));
    assert!(
        db.rows("SELECT github_login FROM users WHERE id = 'usr_old'")[0]["github_login"].is_null()
    );

    // Just over seven days is already too old.
    db.exec("INSERT INTO users (id, display_name, github_login) VALUES ('usr_edge', 'E', 'Edge')");
    age_user(&db, "usr_edge", T0 + 10, 7 * DAY_MS + 10);
    let edge = sign_in(&db, &alice(5003, "edge"), LOOPBACK, T0 + 10);
    assert_eq!(param_of(&edge, "error").as_deref(), Some("access_denied"));
}

// ---- the bootstrap owner's login (BOOTSTRAP_OWNER_GITHUB_LOGIN) --------------------------------

/// [`sign_in`] with the given config (the bootstrap login is a config value, not a seed).
fn sign_in_with(
    db: &Fake,
    config: &GithubConfig,
    user: &GithubUser,
    redirect_uri: &str,
    now: i64,
) -> String {
    let url = start(db, &query(redirect_uri), now).expect("authorize");
    let pending = consume(db, &url, now).expect("pending");
    block_on(finish_callback(db, config, &pending, user, now)).expect("finish")
}

#[test]
fn the_bootstrap_owner_is_linked_by_the_configured_login_whatever_its_age_and_only_once() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_boot");
    audit_bootstrap(&db, "org_a", "usr_boot");
    // Far past the 7-day rule: only the operator-set login can link this owner.
    age_user(&db, "usr_boot", T0, 30 * DAY_MS);
    let config = config_with_bootstrap("boss");

    let location = sign_in_with(&db, &config, &alice(9001, "Boss"), LOOPBACK, T0);
    let code = param_of(&location, "code").expect("the bootstrap owner is linked");
    let granted = token(&db, &code, VERIFIER, LOOPBACK, T0).expect("token");
    assert_eq!(granted["user"]["id"], json!("usr_boot"));
    assert_eq!(granted["role"], json!("owner"));
    assert_eq!(
        db.rows("SELECT github_id FROM users WHERE id = 'usr_boot'")[0]["github_id"],
        json!(9001)
    );
    // Audited as a link, and by which rule.
    let audited = db.rows("SELECT detail_json FROM audit_log WHERE action = 'auth.github.link'");
    assert_eq!(audited.len(), 1);
    assert!(
        audited[0]["detail_json"]
            .as_str()
            .expect("detail")
            .contains("bootstrap_owner_login")
    );

    // Once: the same account signs in as the owner by its GitHub id, with no second link.
    let again = sign_in_with(&db, &config, &alice(9001, "boss"), LOOPBACK, T0 + 1);
    let code = param_of(&again, "code").expect("the owner signs in again");
    assert_eq!(
        token(&db, &code, VERIFIER, LOOPBACK, T0 + 1).expect("token")["user"]["id"],
        json!("usr_boot")
    );
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM audit_log WHERE action = 'auth.github.link'"),
        1
    );
    // Another GitHub account with the same login gets nothing: the owner already has an id.
    let other = sign_in_with(&db, &config, &alice(9002, "boss"), LOOPBACK, T0 + 2);
    assert_eq!(param_of(&other, "error").as_deref(), Some("access_denied"));
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM users WHERE github_id = 9002"),
        0
    );
}

#[test]
fn no_other_login_can_use_the_bootstrap_owner_link() {
    let db = Fake::new();
    seed_org(&db, "org_a", "usr_boot");
    audit_bootstrap(&db, "org_a", "usr_boot");
    // Another admin-made user, old enough to be refused by the 7-day rule anyway.
    db.exec(
        "INSERT INTO users (id, display_name, github_login) VALUES ('usr_pal', 'Pal', 'Pal');
         INSERT INTO members (org_id, user_id, role) VALUES ('org_a', 'usr_pal', 'admin');",
    );
    age_user(&db, "usr_pal", T0, 30 * DAY_MS);
    age_user(&db, "usr_boot", T0, 30 * DAY_MS);
    let config = config_with_bootstrap("boss");

    // A login that is not the configured one, and the configured one of a non-owner user.
    for (id, login) in [(9101, "mallory"), (9102, "pal")] {
        let location = sign_in_with(&db, &config, &alice(id, login), LOOPBACK, T0);
        assert_eq!(
            param_of(&location, "error").as_deref(),
            Some("access_denied"),
            "{login}"
        );
    }
    // With no BOOTSTRAP_OWNER_GITHUB_LOGIN at all, the configured login means nothing either.
    let unset = sign_in(&db, &alice(9103, "boss"), LOOPBACK, T0);
    assert_eq!(param_of(&unset, "error").as_deref(), Some("access_denied"));

    assert!(db.rows("SELECT github_id FROM users WHERE id = 'usr_boot'")[0]["github_id"].is_null());
    assert!(db.rows("SELECT github_id FROM users WHERE id = 'usr_pal'")[0]["github_id"].is_null());
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM users WHERE github_id IN (9101, 9102, 9103)"),
        0
    );
}

#[test]
fn the_bootstrap_link_needs_the_bootstrap_audit_row_and_an_owner_without_a_github_id() {
    let db = Fake::new();
    // A login-named user in an org that was never bootstrapped by this owner: no audit row.
    seed_org(&db, "org_a", "usr_plain");
    db.exec("UPDATE users SET github_login = 'boss' WHERE id = 'usr_plain'");
    age_user(&db, "usr_plain", T0, 30 * DAY_MS);
    let config = config_with_bootstrap("boss");
    let refused = sign_in_with(&db, &config, &alice(9201, "boss"), LOOPBACK, T0);
    assert_eq!(
        param_of(&refused, "error").as_deref(),
        Some("access_denied")
    );
    assert!(
        db.rows("SELECT github_id FROM users WHERE id = 'usr_plain'")[0]["github_id"].is_null()
    );

    // The bootstrap owner, but already holding a GitHub id: nothing is relinked.
    seed_org(&db, "org_b", "usr_boot");
    audit_bootstrap(&db, "org_b", "usr_boot");
    db.exec("UPDATE users SET github_id = 4444 WHERE id = 'usr_boot'");
    let held = sign_in_with(&db, &config, &alice(9202, "boss"), LOOPBACK, T0);
    assert_eq!(param_of(&held, "error").as_deref(), Some("access_denied"));
    assert_eq!(
        db.rows("SELECT github_id FROM users WHERE id = 'usr_boot'")[0]["github_id"],
        json!(4444)
    );
}

// ---- GitHub replies --------------------------------------------------------------------------

#[test]
fn github_replies_are_parsed_strictly() {
    assert_eq!(
        parse_token_response(r#"{"access_token":"gho_x","token_type":"bearer"}"#).unwrap(),
        "gho_x"
    );
    for bad in [
        r#"{"error":"bad_verification_code"}"#,
        r#"{"access_token":""}"#,
        "not json",
        "",
    ] {
        assert_eq!(
            parse_token_response(bad).expect_err(bad).status,
            502,
            "{bad}"
        );
    }
    let user = parse_user(r#"{"id":42,"login":"octo-cat","name":"Octo","email":null}"#).unwrap();
    assert_eq!(
        user,
        GithubUser {
            id: 42,
            login: "octo-cat".into(),
            name: Some("Octo".into()),
            email: None
        }
    );
    for bad in [
        r#"{"login":"x"}"#,
        r#"{"id":0,"login":"x"}"#,
        r#"{"id":"7","login":"x"}"#,
        r#"{"id":1,"login":"-x"}"#,
        r#"{"id":1,"login":"a b"}"#,
        r#"{"id":1}"#,
        "[]",
    ] {
        assert_eq!(parse_user(bad).expect_err(bad).status, 502, "{bad}");
    }
}

#[test]
fn s256_matches_the_rfc_7636_example() {
    // RFC 7636 appendix B.
    assert_eq!(
        s256_challenge("dBjftJeZ4CVP-mB92K27uhbUJU1p1r_wW1gFWFOEjXk"),
        "E9Melhoa2OwvFrEMTJguCHaoeK1t8URWbuGJSstw-cM"
    );
}
