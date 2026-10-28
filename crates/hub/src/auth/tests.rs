//! Native tests of the auth logic over an in-memory SQLite that runs the real migrations (the
//! same pattern as `tests/d1_schema.rs`). The live tests in `tests/authz.rs` cover the HTTP
//! surface; these cover the cases a live Worker cannot reach (`DEV_FIXTURES` off, expiry,
//! concurrency of the bootstrap claim).

use std::cell::RefCell;
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::service::{authorize_thread, bootstrap, issue_session, revoke_current, revoke_other};
use super::token::hash_token;
use super::{Principal, authenticate};
use crate::db::{Db, DbError, MIGRATIONS, Param, Role, Row};

struct Fake {
    conn: RefCell<rusqlite::Connection>,
}

impl Fake {
    fn new() -> Self {
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

    fn rows(&self, sql: &str) -> Vec<Row> {
        run_query(&self.conn.borrow(), sql, &[]).expect("query")
    }

    fn exec(&self, sql: &str) {
        self.conn.borrow().execute_batch(sql).expect("exec");
    }
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

impl Db for Fake {
    fn execute(&self, sql: &str, params: &[Param]) -> impl Future<Output = Result<u64, DbError>> {
        let conn = self.conn.borrow();
        let result = conn
            .prepare(sql)
            .and_then(|mut s| s.execute(params_from_iter(bind(params))))
            .map(|n| n as u64)
            .map_err(backend);
        ready(result)
    }

    fn query(
        &self,
        sql: &str,
        params: &[Param],
    ) -> impl Future<Output = Result<Vec<Row>, DbError>> {
        ready(run_query(&self.conn.borrow(), sql, params))
    }
}

fn block_on<T>(future: impl Future<Output = T>) -> T {
    let mut future = pin!(future);
    let mut cx = Context::from_waker(Waker::noop());
    match future.as_mut().poll(&mut cx) {
        Poll::Ready(value) => value,
        Poll::Pending => panic!("future pending: the fake never yields"),
    }
}

const SECRET: &str = "test-bootstrap-secret";

fn bearer(token: &str) -> String {
    format!("Bearer {token}")
}

fn do_bootstrap(db: &Fake, token: &str) -> Result<Value, crate::threads::ApiError> {
    block_on(bootstrap(
        db,
        Some(SECRET),
        Some(&bearer(token)),
        &json!({"org_name": "Acme Inc", "user_name": "Ada", "user_email": "ada@example.com"}),
        "req_1",
    ))
}

fn bootstrapped() -> (Fake, Principal, String) {
    let db = Fake::new();
    let out = do_bootstrap(&db, SECRET).expect("bootstrap");
    let token = out["session"]["token"].as_str().expect("token").to_owned();
    let owner = block_on(authenticate(&db, Some(&bearer(&token)), false)).expect("owner");
    (db, owner, token)
}

fn issue(db: &Fake, owner: &Principal, role: &str) -> (Principal, String) {
    let out = block_on(issue_session(db, owner, &json!({"role": role}), "req_i")).expect("issue");
    let token = out["session"]["token"].as_str().expect("token").to_owned();
    let principal = block_on(authenticate(db, Some(&bearer(&token)), false)).expect("auth");
    (principal, token)
}

#[test]
fn bootstrap_creates_owner_once_and_stores_only_hashes() {
    let (db, owner, token) = bootstrapped();
    assert_eq!(owner.role, Role::Owner);
    assert_eq!(owner.display_name, "Ada");
    assert!(token.starts_with("smt_") && token.len() == 68);

    let second = do_bootstrap(&db, SECRET).expect_err("second bootstrap");
    assert_eq!(second.status, 409);

    let stored = db.rows("SELECT token_hash FROM sessions");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["token_hash"], hash_token(&token));
    assert_ne!(stored[0]["token_hash"], token.as_str());
    // The bootstrap secret and the token are nowhere in the audit log.
    let audit = db.rows("SELECT action, detail_json, request_id FROM audit_log");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["action"], "auth.bootstrap");
    assert_eq!(audit[0]["request_id"], "req_1");
    let dump = format!("{audit:?}");
    assert!(!dump.contains(SECRET) && !dump.contains(&token));
}

#[test]
fn wrong_or_missing_bootstrap_token_is_401_and_changes_nothing() {
    let db = Fake::new();
    assert_eq!(do_bootstrap(&db, "nope").expect_err("wrong").status, 401);
    let missing = block_on(bootstrap(&db, Some(SECRET), None, &json!({}), "r"));
    assert_eq!(missing.expect_err("missing").status, 401);
    let unset = block_on(bootstrap(&db, None, Some(&bearer(SECRET)), &json!({}), "r"));
    assert_eq!(unset.expect_err("unset secret").status, 401);
    assert!(db.rows("SELECT id FROM orgs").is_empty());
    assert!(db.rows("SELECT id FROM auth_bootstrap").is_empty());
    // A wrong token after a successful bootstrap is still 401, not 409.
    do_bootstrap(&db, SECRET).expect("bootstrap");
    assert_eq!(do_bootstrap(&db, "nope").expect_err("wrong").status, 401);
}

#[test]
fn a_claimed_bootstrap_loses_the_race_with_409() {
    let db = Fake::new();
    // Another attempt already holds the claim (it is mid-way): this one must be refused.
    db.exec("INSERT INTO auth_bootstrap (id) VALUES (1)");
    assert_eq!(do_bootstrap(&db, SECRET).expect_err("lost").status, 409);
    assert!(db.rows("SELECT id FROM orgs").is_empty());
}

#[test]
fn a_failed_bootstrap_releases_the_claim() {
    let db = Fake::new();
    // Make the org insert fail after the claim: a trigger aborts it.
    db.exec("CREATE TRIGGER boom BEFORE INSERT ON orgs BEGIN SELECT RAISE(ABORT, 'boom'); END");
    assert_eq!(do_bootstrap(&db, SECRET).expect_err("fails").status, 500);
    assert!(db.rows("SELECT id FROM auth_bootstrap").is_empty());
    db.exec("DROP TRIGGER boom");
    do_bootstrap(&db, SECRET).expect("works after the failure is gone");
}

#[test]
fn sessions_are_honoured_until_expired_or_revoked() {
    let (db, owner, _) = bootstrapped();
    let (member, token) = issue(&db, &owner, "member");
    assert_eq!(member.role, Role::Member);

    // Expired.
    db.exec(&format!(
        "UPDATE sessions SET expires_at = '2000-01-01T00:00:00.000Z' WHERE token_hash = '{}'",
        hash_token(&token)
    ));
    let error = block_on(authenticate(&db, Some(&bearer(&token)), false)).expect_err("expired");
    assert_eq!((error.status, error.code), (401, "unauthorized"));

    // Revoked.
    let (viewer, vtoken) = issue(&db, &owner, "viewer");
    block_on(revoke_current(&db, &viewer, "req_r")).expect("revoke");
    let error = block_on(authenticate(&db, Some(&bearer(&vtoken)), false)).expect_err("revoked");
    assert_eq!(error.status, 401);

    // Garbage and lookalikes.
    for header in [
        None,
        Some("Bearer "),
        Some("Bearer smt_00"),
        Some("Bearer other"),
        Some("Basic abc"),
    ] {
        assert_eq!(
            block_on(authenticate(&db, header, true))
                .expect_err("rejected")
                .status,
            401
        );
    }
}

#[test]
fn a_revoked_device_or_removed_member_loses_access() {
    let (db, owner, _) = bootstrapped();
    let (member, token) = issue(&db, &owner, "member");
    db.exec(&format!(
        "UPDATE devices SET revoked_at = '2020-01-01T00:00:00.000Z' WHERE id = '{}'",
        member.device_id
    ));
    assert_eq!(
        block_on(authenticate(&db, Some(&bearer(&token)), false))
            .expect_err("device revoked")
            .status,
        401
    );
    let (second, token2) = issue(&db, &owner, "member");
    db.exec(&format!(
        "DELETE FROM members WHERE user_id = '{}'",
        second.user_id
    ));
    assert_eq!(
        block_on(authenticate(&db, Some(&bearer(&token2)), false))
            .expect_err("not a member")
            .status,
        401
    );
}

#[test]
fn bearer_dev_needs_dev_fixtures_and_is_an_owner() {
    let db = Fake::new();
    let off = block_on(authenticate(&db, Some("Bearer dev"), false)).expect_err("off");
    assert_eq!((off.status, off.code), (401, "unauthorized"));
    assert!(db.rows("SELECT id FROM users").is_empty());

    let dev = block_on(authenticate(&db, Some("Bearer dev"), true)).expect("dev");
    assert_eq!((dev.role, dev.org_id.as_str()), (Role::Owner, "org_dev"));
    assert_eq!(dev.actor().id, "usr_dev");
    assert_eq!(dev.actor().device_id.as_deref(), Some("dev_dev"));
    assert_eq!(dev.actor().name, None);
    // Created once, usable again.
    block_on(authenticate(&db, Some("Bearer dev"), true)).expect("dev again");
    assert_eq!(db.rows("SELECT id FROM orgs").len(), 1);
}

#[test]
fn issue_rules_roles_ttl_and_audit() {
    let (db, owner, _) = bootstrapped();
    let (member, _) = issue(&db, &owner, "member");
    let (admin, _) = issue(&db, &owner, "admin");

    let denied = block_on(issue_session(&db, &member, &json!({}), "r")).expect_err("member");
    assert_eq!((denied.status, denied.code), (403, "forbidden"));
    let denied = block_on(issue_session(&db, &admin, &json!({"role": "admin"}), "r"))
        .expect_err("admin grants admin");
    assert_eq!(denied.status, 403);
    block_on(issue_session(&db, &admin, &json!({"role": "viewer"}), "r")).expect("admin->viewer");
    for bad in [
        json!({"role": "root"}),
        json!({"ttl_seconds": 0}),
        json!({"ttl_seconds": 99999999999i64}),
        json!({"display_name": 5}),
    ] {
        let error = block_on(issue_session(&db, &owner, &bad, "r")).expect_err("bad body");
        assert_eq!(error.status, 400, "{bad}");
    }
    let rows = db.rows("SELECT detail_json FROM audit_log WHERE action = 'auth.session.issue'");
    assert_eq!(rows.len(), 3);
    assert!(!format!("{rows:?}").contains("smt_"));
}

#[test]
fn revoking_others_needs_admin_and_stays_in_the_org() {
    let (db, owner, _) = bootstrapped();
    let (member, _) = issue(&db, &owner, "member");
    let (viewer, _) = issue(&db, &owner, "viewer");
    let viewer_session = viewer.session_id.clone().expect("session");

    let denied = block_on(revoke_other(&db, &member, &viewer_session, "r")).expect_err("member");
    assert_eq!(denied.status, 403);
    let missing = block_on(revoke_other(&db, &owner, "ses_nope", "r")).expect_err("missing");
    assert_eq!(missing.status, 404);
    block_on(revoke_other(&db, &owner, &viewer_session, "r")).expect("admin revokes");
    assert!(
        db.rows(&format!(
            "SELECT revoked_at FROM sessions WHERE id = '{viewer_session}'"
        ))[0]["revoked_at"]
            .is_string()
    );

    // `Bearer dev` has no session to revoke.
    let dev = block_on(authenticate(&db, Some("Bearer dev"), true)).expect("dev");
    assert_eq!(
        block_on(revoke_current(&db, &dev, "r"))
            .expect_err("no session")
            .status,
        400
    );
}

#[test]
fn thread_routes_enforce_roles_and_audit_mutations() {
    let (db, owner, _) = bootstrapped();
    let (_, member_token) = issue(&db, &owner, "member");
    let (_, viewer_token) = issue(&db, &owner, "viewer");
    host_thread(&db, "t1", &owner.org_id);
    let go = |token: &str, method: &str, path: &str| {
        block_on(authorize_thread(
            &db,
            Some(&bearer(token)),
            false,
            method,
            path,
            "req_t",
        ))
    };

    // Viewer: reads fine, writes 403 and unaudited.
    assert!(go(&viewer_token, "GET", "/v1/threads/t1").is_ok());
    for (method, path) in [
        ("POST", "/v1/threads/t1/prompts"),
        ("POST", "/v1/threads/t1/cancel"),
        ("POST", "/v1/threads/t1/interrupts/i1"),
        ("DELETE", "/v1/threads/t1/pending/k1"),
    ] {
        let error = go(&viewer_token, method, path).expect_err("viewer write");
        assert_eq!((error.status, error.code), (403, "forbidden"), "{path}");
    }
    assert!(
        db.rows("SELECT id FROM audit_log WHERE action LIKE 'thread.%'")
            .is_empty()
    );

    // Member: may send, approve, cancel and remove a queued task (M2-008k).
    for (method, path, action) in [
        ("POST", "/v1/threads/t1/prompts", "thread.prompt"),
        ("POST", "/v1/threads/t1/cancel", "thread.cancel"),
        (
            "POST",
            "/v1/threads/t1/interrupts/i1",
            "thread.approval.resolve",
        ),
        (
            "DELETE",
            "/v1/threads/t1/pending/k1",
            "thread.pending.delete",
        ),
    ] {
        let (principal, _) = go(&member_token, method, path).expect("member write");
        assert_eq!(principal.role, Role::Member);
        let rows = db.rows(&format!(
            "SELECT actor_user_id, target_id, request_id FROM audit_log WHERE action = '{action}'"
        ));
        assert_eq!(rows.len(), 1, "{action}");
        assert_eq!(rows[0]["actor_user_id"], principal.user_id.as_str());
        assert_eq!(rows[0]["target_id"], "t1");
        assert_eq!(rows[0]["request_id"], "req_t");
    }

    // Owner deletes too; bad thread ids and unknown routes are refused before any audit row.
    let owner_token = {
        let (_, token) = issue(&db, &owner, "admin");
        token
    };
    go(&owner_token, "DELETE", "/v1/threads/t1/pending/k1").expect("admin delete");
    let rows = db.rows("SELECT detail_json FROM audit_log WHERE action = 'thread.pending.delete'");
    assert_eq!(rows.len(), 2);
    assert!(
        rows[0]["detail_json"]
            .as_str()
            .expect("text")
            .contains("k1")
    );
    let before = db.rows("SELECT id FROM audit_log").len();
    assert_eq!(
        go(&owner_token, "POST", "/v1/threads/bad%20id/prompts")
            .expect_err("bad id")
            .status,
        400
    );
    assert_eq!(
        go(&owner_token, "PUT", "/v1/threads/t1/prompts")
            .expect_err("unknown")
            .status,
        404
    );
    assert_eq!(db.rows("SELECT id FROM audit_log").len(), before);
}

/// Records `thread` as hosted for `org_id` (what thread creation, `thread_open` and the dev
/// init route do).
fn host_thread(db: &Fake, thread: &str, org_id: &str) {
    assert!(
        block_on(crate::db::queries::claim_thread_host(
            db, thread, org_id, "mach_1"
        ))
        .expect("claim")
    );
}

/// A second org, and an owner-role principal of it (to issue its members' sessions).
fn second_org(db: &Fake, owner_a: &Principal) -> Principal {
    db.exec("INSERT INTO orgs (id, slug, name) VALUES ('org_b', 'other', 'Other')");
    Principal {
        org_id: "org_b".to_owned(),
        ..owner_a.clone()
    }
}

#[test]
fn thread_routes_are_scoped_to_the_callers_org() {
    let (db, owner, owner_token) = bootstrapped();
    let (_, member_token) = issue(&db, &owner, "member");
    let other_owner = second_org(&db, &owner);
    let issue_b = |role: &str| {
        let out = block_on(issue_session(
            &db,
            &other_owner,
            &json!({ "role": role }),
            "req_b",
        ))
        .expect("issue in org b");
        out["session"]["token"].as_str().expect("token").to_owned()
    };
    let (other_viewer, other_member, other_admin) =
        (issue_b("viewer"), issue_b("member"), issue_b("admin"));
    host_thread(&db, "th_a", &owner.org_id);
    let go = |token: &str, method: &str, path: &str| {
        block_on(authorize_thread(
            &db,
            Some(&bearer(token)),
            false,
            method,
            path,
            "req_t",
        ))
    };
    let routes = [
        ("GET", "/v1/threads/th_a"),
        ("GET", "/v1/threads/th_a/subscribe"),
        ("GET", "/v1/threads/th_a/pending"),
        ("POST", "/v1/threads/th_a/prompts"),
        ("POST", "/v1/threads/th_a/cancel"),
        ("POST", "/v1/threads/th_a/interrupts/i1"),
        ("POST", "/v1/threads/th_a/session"),
        ("DELETE", "/v1/threads/th_a/pending/k1"),
    ];
    // Org A: every route works for its own members.
    for (method, path) in routes {
        let token = if method == "DELETE" {
            &owner_token
        } else {
            &member_token
        };
        go(token, method, path).unwrap_or_else(|e| panic!("{method} {path}: {e:?}"));
    }
    let audited = db
        .rows("SELECT id FROM audit_log WHERE action LIKE 'thread.%'")
        .len();
    // Org B (a viewer for reads, a member for writes): 404 everywhere, and nothing audited.
    for (method, path) in routes {
        let token = match method {
            "GET" => &other_viewer,
            "DELETE" => &other_admin,
            _ => &other_member,
        };
        let error = go(token, method, path).expect_err(path);
        assert_eq!(
            (error.status, error.code),
            (404, "not_found"),
            "{method} {path}"
        );
    }
    // An unknown thread is the same 404.
    let error = go(&member_token, "POST", "/v1/threads/th_nope/prompts").expect_err("unknown");
    assert_eq!((error.status, error.code), (404, "not_found"));
    assert_eq!(
        db.rows("SELECT id FROM audit_log WHERE action LIKE 'thread.%'")
            .len(),
        audited,
        "a refused request writes no audit row"
    );
}

#[test]
fn a_thread_host_is_claimed_once_and_idempotently() {
    use crate::db::queries::{claim_thread_host, thread_org};
    let (db, owner, _) = bootstrapped();
    assert_eq!(block_on(thread_org(&db, "th_1")).expect("read"), None);
    assert!(block_on(claim_thread_host(&db, "th_1", &owner.org_id, "m1")).expect("claim"));
    // Same org again: fine, and the first machine stays.
    assert!(block_on(claim_thread_host(&db, "th_1", &owner.org_id, "m2")).expect("again"));
    // Another org: refused, mapping unchanged.
    assert!(!block_on(claim_thread_host(&db, "th_1", "org_other", "m3")).expect("foreign"));
    assert_eq!(
        block_on(thread_org(&db, "th_1")).expect("read").as_deref(),
        Some(owner.org_id.as_str())
    );
    assert_eq!(
        db.rows("SELECT machine_id FROM thread_hosts WHERE thread_id = 'th_1'")[0]["machine_id"],
        "m1"
    );
}

#[test]
fn the_session_route_needs_a_member_and_names_thread_session() {
    use super::policy::thread_policy;
    let (db, owner, _) = bootstrapped();
    let (viewer, _) = issue(&db, &owner, "viewer");
    let policy =
        thread_policy("POST", "/v1/threads/th_1/session").expect("the session route has a policy");
    assert_eq!(
        (policy.thread_id.as_str(), policy.role, policy.action),
        ("th_1", Role::Member, Some("thread.session"))
    );
    let denied = viewer.require(policy.role).expect_err("viewer");
    assert_eq!((denied.status, denied.code), (403, "forbidden"));
}

#[test]
fn post_threads_needs_a_member_and_names_thread_create() {
    use super::policy::thread_policy;
    let (db, owner, _) = bootstrapped();
    let (viewer, _) = issue(&db, &owner, "viewer");
    let (member, _) = issue(&db, &owner, "member");
    let policy = thread_policy("POST", "/v1/threads").expect("POST /v1/threads has a policy");
    assert_eq!(
        (policy.role, policy.action),
        (Role::Member, Some("thread.create"))
    );
    let denied = viewer.require(policy.role).expect_err("viewer");
    assert_eq!((denied.status, denied.code), (403, "forbidden"));
    member.require(policy.role).expect("member");
    owner.require(policy.role).expect("owner");
    // Only POST: other methods on the collection stay unknown routes.
    assert!(thread_policy("GET", "/v1/threads").is_none());
}

#[test]
fn audit_log_stays_append_only() {
    let (db, _, _) = bootstrapped();
    let conn = db.conn.borrow();
    assert!(conn.execute("DELETE FROM audit_log", []).is_err());
    assert!(
        conn.execute("UPDATE audit_log SET action = 'x'", [])
            .is_err()
    );
}

// ---- subscribe tickets (SPEC-006, M2-008h) ----------------------------------------------

mod subscribe_tickets {
    use super::*;
    use crate::auth::ticket::{self, TICKET_TTL_MS};

    const T0: i64 = 1_700_000_000_000;

    fn minted(db: &Fake, who: &Principal, thread: &str, now: i64) -> String {
        let out = block_on(ticket::mint(db, who, thread, now)).expect("mint");
        out["ticket"].as_str().expect("ticket").to_owned()
    }

    fn consume(db: &Fake, ticket_value: &str, thread: &str, now: i64) -> Result<Principal, u16> {
        block_on(ticket::consume(db, false, ticket_value, thread, now)).map_err(|e| e.status)
    }

    fn setup() -> (Fake, Principal, Principal, String) {
        let (db, owner, _) = bootstrapped();
        let (viewer, viewer_token) = issue(&db, &owner, "viewer");
        host_thread(&db, "t1", &owner.org_id);
        (db, owner, viewer, viewer_token)
    }

    fn host_other_org(db: &Fake) {
        db.exec("INSERT INTO orgs (id, slug, name) VALUES ('org_b', 'b', 'B')");
        assert!(
            block_on(crate::db::queries::claim_thread_host(
                db, "t_other", "org_b", "mach_b"
            ))
            .expect("claim")
        );
    }

    #[test]
    fn a_ticket_is_256_bits_stored_only_as_a_hash_and_expires_in_60_seconds() {
        let (db, _, viewer, _) = setup();
        let out = block_on(ticket::mint(&db, &viewer, "t1", T0)).expect("mint");
        let value = out["ticket"].as_str().expect("ticket");
        assert!(
            value.starts_with("sst_") && value.len() == 4 + 64,
            "{value}"
        );
        assert!(out["expires_at"].as_str().expect("expires").ends_with('Z'));
        let rows = db.rows(
            "SELECT ticket_hash, session_id, user_id, thread_id, expires_at_ms \
             FROM subscribe_tickets",
        );
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["ticket_hash"], hash_token(value));
        assert_eq!(
            rows[0]["session_id"],
            viewer.session_id.as_deref().expect("session")
        );
        assert_eq!(rows[0]["user_id"], viewer.user_id.as_str());
        assert_eq!(rows[0]["thread_id"], "t1");
        assert_eq!(rows[0]["expires_at_ms"], T0 + 60_000);
        assert!(!format!("{rows:?}").contains(value));
        // Never in an audit row either.
        assert!(!format!("{:?}", db.rows("SELECT * FROM audit_log")).contains(value));
        assert_ne!(minted(&db, &viewer, "t1", T0), value);
    }

    #[test]
    fn minting_goes_through_the_viewer_and_org_gate_and_writes_no_audit_row() {
        let (db, _, _, viewer_token) = setup();
        host_other_org(&db);
        let before = db.rows("SELECT id FROM audit_log").len();
        let go = |thread: &str| {
            block_on(authorize_thread(
                &db,
                Some(&bearer(&viewer_token)),
                false,
                "POST",
                &format!("/v1/threads/{thread}/subscribe-ticket"),
                "req_x",
            ))
        };
        let (principal, policy) = go("t1").expect("viewer may mint");
        assert_eq!((principal.role, policy.action), (Role::Viewer, None));
        assert_eq!(db.rows("SELECT id FROM audit_log").len(), before);
        // A thread of another org (or none) is the same 404 as subscribe.
        for thread in ["t_other", "t_unknown"] {
            assert_eq!(go(thread).expect_err("not visible").status, 404, "{thread}");
        }
    }

    #[test]
    fn a_ticket_works_once_for_its_thread_and_its_principal() {
        let (db, _, viewer, _) = setup();
        let value = minted(&db, &viewer, "t1", T0);
        let principal = consume(&db, &value, "t1", T0 + 1).expect("first use");
        assert_eq!(principal, viewer);
        assert_eq!(consume(&db, &value, "t1", T0 + 2), Err(401));
    }

    #[test]
    fn two_racing_consumes_have_exactly_one_winner() {
        let (db, _, viewer, _) = setup();
        let value = minted(&db, &viewer, "t1", T0);
        // Both consumes are in flight together; the single conditional UPDATE is what lets
        // only one of them change the row.
        let (a, b) = block_on(async {
            let first = ticket::consume(&db, false, &value, "t1", T0 + 1);
            let second = ticket::consume(&db, false, &value, "t1", T0 + 1);
            (first.await, second.await)
        });
        assert_eq!(u8::from(a.is_ok()) + u8::from(b.is_ok()), 1, "{a:?} {b:?}");
    }

    #[test]
    fn expired_malformed_unknown_and_other_thread_tickets_are_401() {
        let (db, owner, viewer, _) = setup();
        host_thread(&db, "t2", &owner.org_id);
        let value = minted(&db, &viewer, "t1", T0);
        // Other thread: refused.
        assert_eq!(consume(&db, &value, "t2", T0 + 1), Err(401));
        // Expired exactly at the TTL.
        assert_eq!(consume(&db, &value, "t1", T0 + TICKET_TTL_MS), Err(401));
        let fresh = minted(&db, &viewer, "t1", T0);
        assert!(consume(&db, &fresh, "t1", T0 + TICKET_TTL_MS - 1).is_ok());
        let session_like = format!("smt_{}", "a".repeat(64));
        let unknown = format!("sst_{}", "0".repeat(64));
        for bad in [
            "",
            "sst_",
            "smt_00",
            unknown.as_str(),
            session_like.as_str(),
        ] {
            assert_eq!(consume(&db, bad, "t1", T0 + 1), Err(401), "{bad}");
        }
    }

    #[test]
    fn a_revoked_session_or_device_or_removed_member_loses_its_tickets() {
        let (db, owner, viewer, viewer_token) = setup();
        let value = minted(&db, &viewer, "t1", T0);
        block_on(revoke_current(&db, &viewer, "r")).expect("revoke");
        assert_eq!(consume(&db, &value, "t1", T0 + 1), Err(401));
        // The revoked session cannot mint either.
        assert!(
            block_on(authorize_thread(
                &db,
                Some(&bearer(&viewer_token)),
                false,
                "POST",
                "/v1/threads/t1/subscribe-ticket",
                "r"
            ))
            .is_err()
        );

        let (member, _) = issue(&db, &owner, "member");
        let value = minted(&db, &member, "t1", T0);
        db.exec(&format!(
            "UPDATE devices SET revoked_at = '2020-01-01T00:00:00.000Z' WHERE id = '{}'",
            member.device_id
        ));
        assert_eq!(consume(&db, &value, "t1", T0 + 1), Err(401));

        let (other, _) = issue(&db, &owner, "viewer");
        let value = minted(&db, &other, "t1", T0);
        db.exec(&format!(
            "DELETE FROM members WHERE user_id = '{}'",
            other.user_id
        ));
        assert_eq!(consume(&db, &value, "t1", T0 + 1), Err(401));
    }

    #[test]
    fn a_ticket_for_a_thread_that_moved_org_is_refused() {
        let (db, _, viewer, _) = setup();
        let value = minted(&db, &viewer, "t1", T0);
        db.exec("INSERT INTO orgs (id, slug, name) VALUES ('org_b', 'b', 'B')");
        db.exec("UPDATE thread_hosts SET org_id = 'org_b' WHERE thread_id = 't1'");
        assert_eq!(consume(&db, &value, "t1", T0 + 1), Err(401));
    }

    #[test]
    fn a_dev_ticket_needs_the_dev_fixtures() {
        let db = Fake::new();
        let dev = block_on(authenticate(&db, Some("Bearer dev"), true)).expect("dev");
        host_thread(&db, "t1", &dev.org_id);
        let value = minted(&db, &dev, "t1", T0);
        let refused = block_on(ticket::consume(&db, false, &value, "t1", T0 + 1));
        assert_eq!(refused.expect_err("fixtures off").status, 401);
        let value = minted(&db, &dev, "t1", T0);
        let ok = block_on(ticket::consume(&db, true, &value, "t1", T0 + 1)).expect("dev ticket");
        assert_eq!(ok, dev);
    }

    #[test]
    fn query_helpers_find_and_strip_only_the_ticket() {
        assert_eq!(
            ticket::ticket_in_query(Some("after=3&ticket=sst_ab")),
            Some("sst_ab")
        );
        assert_eq!(ticket::ticket_in_query(Some("after=3")), None);
        assert_eq!(ticket::ticket_in_query(None), None);
        assert_eq!(ticket::strip_ticket("after=3&ticket=sst_ab"), "after=3");
        assert_eq!(ticket::strip_ticket("ticket=x&after=3&ticket=y"), "after=3");
        assert_eq!(ticket::strip_ticket("ticket=x"), "");
    }

    #[test]
    fn minting_sweeps_old_tickets() {
        let (db, _, viewer, _) = setup();
        minted(&db, &viewer, "t1", T0);
        minted(&db, &viewer, "t1", T0 + 10 * TICKET_TTL_MS);
        assert_eq!(db.rows("SELECT id FROM subscribe_tickets").len(), 1);
    }
}
