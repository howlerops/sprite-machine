//! Native tests of the pairing rules over an in-memory SQLite running the real migrations (same
//! pattern as `auth/tests.rs`). The live tests in `tests/devices.rs` cover the HTTP surface;
//! these cover what a live Worker cannot reach: expiry (10 minutes), slow_down arithmetic, the
//! single-use race, a failing Machine DO.

use std::cell::RefCell;
use std::future::{Future, ready};
use std::pin::pin;
use std::task::{Context, Poll, Waker};

use rusqlite::params_from_iter;
use rusqlite::types::Value as SqlValue;
use serde_json::{Value, json};

use super::limits;
use super::service::{self, PollError};
use super::trust;
use crate::auth::{Principal, authenticate};
use crate::db::{Db, DbError, MIGRATIONS, Param, Role, Row};
use crate::machines::admin::MachineControl;
use crate::machines::credential::hash;
use crate::threads::ApiError;

mod signin_tests;

struct Fake {
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

#[derive(Default)]
struct FakeControl {
    provisioned: RefCell<Vec<(String, String, String)>>,
    revoked: RefCell<Vec<String>>,
    fail_provision: bool,
}

impl MachineControl for FakeControl {
    fn provision(
        &self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
    ) -> impl Future<Output = Result<(), ApiError>> {
        self.provisioned.borrow_mut().push((
            machine_id.to_owned(),
            org_id.to_owned(),
            credential_hash.to_owned(),
        ));
        ready(if self.fail_provision {
            Err(ApiError::internal("machine DO unreachable"))
        } else {
            Ok(())
        })
    }

    fn revoke(&self, machine_id: &str) -> impl Future<Output = Result<(), ApiError>> {
        self.revoked.borrow_mut().push(machine_id.to_owned());
        ready(Ok(()))
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

const BASE: &str = "https://hub.example";
const T0: i64 = 1_000_000;
const MINUTE: i64 = 60_000;

fn owner(db: &Fake) -> Principal {
    block_on(authenticate(db, Some("Bearer dev"), true)).expect("dev owner")
}

fn with_role(owner: &Principal, role: Role) -> Principal {
    Principal {
        role,
        ..owner.clone()
    }
}

fn started(db: &Fake, now: i64) -> (String, String) {
    let out = block_on(service::start(db, BASE, None, now)).expect("start");
    (
        out["device_code"].as_str().expect("device_code").to_owned(),
        out["user_code"].as_str().expect("user_code").to_owned(),
    )
}

fn approve(db: &Fake, who: &Principal, user_code: &str, now: i64) -> Result<Value, ApiError> {
    block_on(service::approve(
        db,
        who,
        &json!({ "user_code": user_code, "name": "laptop" }),
        now,
        "req_a",
    ))
}

fn poll(db: &Fake, control: &FakeControl, code: &str, now: i64) -> Result<Value, PollError> {
    block_on(service::poll(db, control, None, code, now))
}

fn rfc(error: &PollError) -> &'static str {
    match error {
        PollError::Rfc { code, .. } => code,
        PollError::Api(error) => panic!("expected an RFC error, got {error}"),
    }
}

#[test]
fn start_answers_the_rfc_fields_and_stores_only_the_hash() {
    let db = Fake::new();
    let out = block_on(service::start(&db, "https://hub.example/", None, T0)).expect("start");
    assert_eq!(out["expires_in"], 600);
    assert_eq!(out["interval"], 5);
    assert_eq!(out["verification_uri"], "https://hub.example/device");
    let user_code = out["user_code"].as_str().expect("user code");
    assert_eq!(user_code.len(), 9);
    assert_eq!(&user_code[4..5], "-");
    assert_eq!(
        out["verification_uri_complete"],
        format!("https://hub.example/device?user_code={user_code}")
    );
    let device_code = out["device_code"].as_str().expect("device code");
    let stored = db.rows("SELECT device_code_hash, user_code, status FROM device_pairings");
    assert_eq!(stored.len(), 1);
    assert_eq!(stored[0]["device_code_hash"], hash(device_code));
    assert_eq!(stored[0]["status"], "pending");
    assert!(!format!("{stored:?}").contains(device_code));
}

#[test]
fn full_flow_pending_then_approved_then_credential_once() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);

    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0).expect_err("pending")),
        "authorization_pending"
    );
    // The typed form is forgiving: lower case, no dash.
    let typed = user_code.replace('-', "").to_lowercase();
    approve(&db, &owner, &typed, T0 + 6_000).expect("approve");

    let granted = poll(&db, &control, &device_code, T0 + 12_000).expect("granted");
    let machine_id = granted["machine_id"].as_str().expect("machine id");
    let credential = granted["credential"].as_str().expect("credential");
    assert!(credential.starts_with("smm_") && credential.len() == 68);
    assert_eq!(granted["name"], "laptop");

    // Exactly like POST /v1/machines: a row with only the hash, the DO told the hash, an audit row.
    let machines = db.rows("SELECT id, credential_hash, origin, org_id FROM machines");
    assert_eq!(machines.len(), 1);
    assert_eq!(machines[0]["id"], machine_id);
    assert_eq!(machines[0]["credential_hash"], hash(credential));
    assert_eq!(machines[0]["origin"], "device");
    assert_eq!(machines[0]["org_id"], owner.org_id.as_str());
    let provisioned = control.provisioned.borrow();
    assert_eq!(provisioned.len(), 1);
    assert_eq!(provisioned[0].0, machine_id);
    assert_eq!(provisioned[0].2, hash(credential));
    let audit = db.rows("SELECT action, detail_json FROM audit_log ORDER BY seq");
    let actions: Vec<_> = audit.iter().map(|r| r["action"].clone()).collect();
    assert_eq!(actions, [json!("device.approve"), json!("device.pair")]);
    assert!(!format!("{audit:?}").contains(credential));

    // Single use: the same device code never yields a second credential.
    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0 + 30_000).expect_err("used")),
        "invalid_grant"
    );
    assert_eq!(db.rows("SELECT id FROM machines").len(), 1);
}

#[test]
fn polling_too_fast_is_slow_down_and_the_interval_grows() {
    let db = Fake::new();
    let control = FakeControl::default();
    let (device_code, _) = started(&db, T0);
    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0).expect_err("first")),
        "authorization_pending"
    );
    let slow = poll(&db, &control, &device_code, T0 + 1_000).expect_err("too fast");
    assert_eq!(
        slow,
        PollError::Rfc {
            code: "slow_down",
            interval: Some(10)
        }
    );
    // 6 s later is fine for the old interval but not the new one (10 s since the last poll).
    let again = poll(&db, &control, &device_code, T0 + 7_000).expect_err("still too fast");
    assert_eq!(
        again,
        PollError::Rfc {
            code: "slow_down",
            interval: Some(15)
        }
    );
    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0 + 7_000 + 15_000).expect_err("ok")),
        "authorization_pending"
    );
}

#[test]
fn expired_after_ten_minutes_even_if_approved() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0 + 10 * MINUTE).expect_err("expired")),
        "expired_token"
    );
    // Approving an expired code is refused too.
    assert_eq!(
        approve(&db, &owner, &user_code, T0 + 10 * MINUTE)
            .expect_err("expired")
            .status,
        404
    );

    let (second, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0 + 9 * MINUTE).expect("approve in time");
    assert_eq!(
        rfc(&poll(&db, &control, &second, T0 + 10 * MINUTE + 1).expect_err("late")),
        "expired_token"
    );
    assert!(db.rows("SELECT id FROM machines").is_empty());
}

#[test]
fn denied_pairing_answers_access_denied_and_creates_nothing() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    block_on(service::deny(
        &db,
        &owner,
        &json!({ "user_code": user_code }),
        T0,
        "req_d",
    ))
    .expect("deny");
    assert_eq!(
        rfc(&poll(&db, &control, &device_code, T0 + 6_000).expect_err("denied")),
        "access_denied"
    );
    // A denied code cannot be approved afterwards.
    assert_eq!(
        approve(&db, &owner, &user_code, T0 + 1)
            .expect_err("answered")
            .status,
        404
    );
    assert!(db.rows("SELECT id FROM machines").is_empty());
}

#[test]
fn unknown_device_code_and_viewer_approval_are_refused() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    assert_eq!(
        rfc(&poll(&db, &control, "sdc_nope", T0).expect_err("unknown")),
        "invalid_grant"
    );
    let (_, user_code) = started(&db, T0);
    let viewer = with_role(&owner, Role::Viewer);
    assert_eq!(
        approve(&db, &viewer, &user_code, T0)
            .expect_err("viewer")
            .status,
        403
    );
    let member = with_role(&owner, Role::Member);
    assert_eq!(
        approve(&db, &member, "AAAA-AAA0", T0)
            .expect_err("bad code")
            .status,
        400
    );
    assert_eq!(
        approve(&db, &member, "ABCD-EFGH", T0)
            .expect_err("unknown")
            .status,
        404
    );
    approve(&db, &member, &user_code, T0).expect("a member may approve");
}

#[test]
fn a_failing_machine_do_leaves_no_row_and_the_pairing_can_be_retried() {
    let db = Fake::new();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0).expect("approve");
    let broken = FakeControl {
        fail_provision: true,
        ..FakeControl::default()
    };
    let error = poll(&db, &broken, &device_code, T0 + 6_000).expect_err("DO down");
    assert!(matches!(error, PollError::Api(ref e) if e.status == 500));
    assert!(db.rows("SELECT id FROM machines").is_empty());
    let working = FakeControl::default();
    let granted = poll(&db, &working, &device_code, T0 + 12_000).expect("retry");
    assert!(granted["credential"].as_str().is_some());
}

#[test]
fn list_revoke_and_policy_reuse_the_machines_registry() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0).expect("approve");
    let granted = poll(&db, &control, &device_code, T0 + 6_000).expect("granted");
    let machine_id = granted["machine_id"].as_str().expect("id").to_owned();

    // An admin-API machine is not a paired device.
    block_on(crate::machines::admin::create_machine(
        &db,
        &control,
        &owner,
        &json!({ "name": "ci" }),
        "req_m",
    ))
    .expect("admin machine");
    let listed = block_on(service::list(&db, &owner)).expect("list");
    let devices = listed["devices"].as_array().expect("devices");
    assert_eq!(devices.len(), 1);
    assert_eq!(devices[0]["machine_id"], machine_id.as_str());
    assert_eq!(listed["require_trusted_devices"], false);

    let admin_id = db.rows("SELECT id FROM machines WHERE origin = 'admin'")[0]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    let not_a_device = block_on(service::revoke(&db, &control, &owner, &admin_id, "r"));
    assert_eq!(not_a_device.expect_err("not a device").status, 404);

    let member = with_role(&owner, Role::Member);
    let forbidden = block_on(service::revoke(&db, &control, &member, &machine_id, "r"));
    assert_eq!(forbidden.expect_err("member").status, 403);
    block_on(service::revoke(&db, &control, &owner, &machine_id, "r")).expect("revoke");
    assert_eq!(
        control.revoked.borrow().as_slice(),
        std::slice::from_ref(&machine_id)
    );
    assert!(!block_on(trust::machine_may_connect(&db, &machine_id)).expect("trust"));

    // The policy: admin only, stored on the org, shown by the list.
    let bad = block_on(service::set_policy(
        &db,
        &control,
        false,
        &owner,
        &json!({}),
        "r",
    ));
    assert_eq!(bad.expect_err("shape").status, 400);
    let denied = block_on(service::set_policy(
        &db,
        &control,
        false,
        &member,
        &json!({ "require_trusted_devices": true }),
        "r",
    ));
    assert_eq!(denied.expect_err("member").status, 403);
    block_on(service::set_policy(
        &db,
        &control,
        false,
        &owner,
        &json!({ "require_trusted_devices": true }),
        "r",
    ))
    .expect("policy");
    let listed = block_on(service::list(&db, &owner)).expect("list");
    assert_eq!(listed["require_trusted_devices"], true);
    // With the setting on, the admin-API machine is still allowed to connect.
    assert!(block_on(trust::machine_may_connect(&db, &admin_id)).expect("trust"));
}

// ---- M2B-005a: trust gate, rate limits, purge, approver identity, self revoke --------------

fn verdict(db: &Fake, machine_id: &str, dev: bool) -> trust::Verdict {
    block_on(trust::tunnel_verdict(db, machine_id, dev)).expect("verdict")
}

fn set_trusted(db: &Fake, owner: &Principal, on: bool) {
    set_trusted_with(db, &FakeControl::default(), false, owner, on);
}

fn set_trusted_with(db: &Fake, control: &FakeControl, dev: bool, owner: &Principal, on: bool) {
    block_on(service::set_policy(
        db,
        control,
        dev,
        owner,
        &json!({ "require_trusted_devices": on }),
        "r",
    ))
    .expect("policy");
}

#[test]
fn the_tunnel_verdict_follows_origin_revocation_and_the_org_setting() {
    use trust::Verdict::{Allowed, Revoked, Untrusted};
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let admin_machine = block_on(crate::machines::admin::create_machine(
        &db,
        &control,
        &owner,
        &json!({ "name": "ci" }),
        "r",
    ))
    .expect("machine");
    let admin_id = admin_machine["machine"]["id"]
        .as_str()
        .expect("id")
        .to_owned();
    assert_eq!(verdict(&db, &admin_id, false), Allowed);
    set_trusted(&db, &owner, true);
    // Admin-issued and device machines stay allowed with the setting on.
    assert_eq!(verdict(&db, &admin_id, false), Allowed);
    let (device_code, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0).expect("approve");
    let granted = poll(&db, &control, &device_code, T0 + 6_000).expect("granted");
    let device_id = granted["machine_id"].as_str().expect("id").to_owned();
    assert_eq!(verdict(&db, &device_id, false), Allowed);
    assert!(block_on(trust::machine_may_connect(&db, &device_id)).expect("may connect"));

    // Not vouched for (an origin this code does not know): refused with the setting on only.
    db.conn
        .borrow()
        .execute_batch("PRAGMA ignore_check_constraints = ON")
        .expect("pragma");
    db.conn
        .borrow()
        .execute(
            "UPDATE machines SET origin = 'self' WHERE id = ?",
            [&device_id],
        )
        .expect("unvouch");
    assert_eq!(verdict(&db, &device_id, false), Untrusted);
    assert!(!block_on(trust::machine_may_connect(&db, &device_id)).expect("may connect"));
    set_trusted(&db, &owner, false);
    assert_eq!(verdict(&db, &device_id, false), Allowed);
    set_trusted(&db, &owner, true);

    // A revoked machine goes to the DO, which closes it with auth_revoked.
    block_on(crate::machines::admin::revoke_machine(
        &db, &control, &owner, &admin_id, "r",
    ))
    .expect("revoke");
    assert_eq!(verdict(&db, &admin_id, false), Revoked);

    // No row: refused, except under dev fixtures where the dev org's setting decides.
    assert_eq!(verdict(&db, "mch_nowhere", false), Untrusted);
    assert_eq!(
        verdict(&db, "mch_nowhere", true),
        Untrusted,
        "the dev org requires trusted devices"
    );
    set_trusted(&db, &owner, false);
    assert_eq!(verdict(&db, "mch_nowhere", true), Allowed);
}

fn stranger(owner: &Principal, user: &str) -> Principal {
    Principal {
        user_id: user.to_owned(),
        ..owner.clone()
    }
}

fn miss(db: &Fake, who: &Principal, now: i64) -> u16 {
    approve(db, who, "ABCD-EFGH", now)
        .expect_err("no such pairing")
        .status
}

#[test]
fn failed_approvals_are_limited_per_minute_and_come_back_after_the_window() {
    let db = Fake::new();
    let owner = owner(&db);
    for i in 0..5 {
        assert_eq!(miss(&db, &owner, T0 + i), 404);
    }
    let limited = approve(&db, &owner, "ABCD-EFGH", T0 + 10).expect_err("limited");
    assert_eq!(limited.status, 429);
    assert_eq!(limits::retry_after(&limited), Some(60));
    // A correct code is refused too while limited (no oracle), and does not use the pairing.
    let (_, user_code) = started(&db, T0);
    assert_eq!(
        approve(&db, &owner, &user_code, T0 + 20)
            .expect_err("limited")
            .status,
        429
    );
    assert_eq!(
        db.rows("SELECT status FROM device_pairings")[0]["status"],
        "pending"
    );
    // 60 s after the oldest failure the window has moved on.
    approve(&db, &owner, &user_code, T0 + MINUTE + 1).expect("allowed again");
}

#[test]
fn failed_approvals_are_limited_per_hour() {
    let db = Fake::new();
    let owner = owner(&db);
    // 4 misses every 2 minutes stay under the minute limit; five rounds reach 20 per hour.
    for round in 0..5 {
        for i in 0..4 {
            assert_eq!(miss(&db, &owner, T0 + round * 2 * MINUTE + i), 404);
        }
    }
    let limited = approve(&db, &owner, "ABCD-EFGH", T0 + 11 * MINUTE).expect_err("hourly");
    assert_eq!(limited.status, 429);
    let wait = limits::retry_after(&limited).expect("retry-after");
    assert!(wait > 60, "the hour window, not the minute one: {wait}");
    assert_eq!(miss(&db, &owner, T0 + 61 * MINUTE), 404);
}

#[test]
fn the_org_limit_spans_users_and_a_success_resets_nobody() {
    let db = Fake::new();
    let owner = owner(&db);
    let other = stranger(&owner, "usr_other");
    for i in 0..3 {
        assert_eq!(miss(&db, &owner, T0 + i), 404);
    }
    for i in 0..2 {
        assert_eq!(miss(&db, &other, T0 + 10 + i), 404);
    }
    // Five failures in the org: both users are limited, though each made fewer than five.
    assert_eq!(miss(&db, &owner, T0 + 20), 429);
    assert_eq!(miss(&db, &other, T0 + 20), 429);

    // Another user's success does not clear anyone's counter.
    let db = Fake::new();
    let owner = self::owner(&db);
    let other = stranger(&owner, "usr_other");
    for i in 0..4 {
        assert_eq!(miss(&db, &other, T0 + i), 404);
    }
    let (_, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0 + 10).expect("owner succeeds");
    // The org counter (4) was not reset by the success: one more miss reaches 5.
    assert_eq!(miss(&db, &other, T0 + 20), 404);
    assert_eq!(miss(&db, &other, T0 + 21), 429);
    assert_eq!(miss(&db, &owner, T0 + 22), 429);
    assert_eq!(
        db.rows("SELECT scope FROM device_attempts WHERE scope = 'approve:user:usr_other'")
            .len(),
        5
    );
}

#[test]
fn deny_shares_the_guessing_limit() {
    let db = Fake::new();
    let owner = owner(&db);
    let deny = |now| {
        block_on(service::deny(
            &db,
            &owner,
            &json!({ "user_code": "ABCD-EFGH" }),
            now,
            "r",
        ))
        .expect_err("no pairing")
        .status
    };
    for i in 0..5 {
        assert_eq!(deny(T0 + i), 404);
    }
    assert_eq!(deny(T0 + 6), 429);
    assert_eq!(miss(&db, &owner, T0 + 7), 429);
}

#[test]
fn starting_is_limited_per_ip_and_by_the_number_of_live_pairings() {
    let db = Fake::new();
    let start = |ip: Option<&str>, now| block_on(service::start(&db, BASE, ip, now));
    for i in 0..10 {
        start(Some("203.0.113.7"), T0 + i).expect("within the limit");
    }
    let limited = start(Some("203.0.113.7"), T0 + 20).expect_err("11th in a minute");
    assert_eq!(limited.status, 429);
    assert_eq!(limits::retry_after(&limited), Some(60));
    start(Some("203.0.113.8"), T0 + 20).expect("another address");
    start(Some("203.0.113.7"), T0 + MINUTE + 1).expect("the window moved on");

    // The cap on live pairings, whoever asks (no IP header).
    let db = Fake::new();
    for i in 0..limits::MAX_PENDING {
        db.conn
            .borrow()
            .execute(
                "INSERT INTO device_pairings (id, device_code_hash, user_code, created_at_ms, \
                 expires_at_ms, interval_seconds) VALUES (?, ?, ?, ?, ?, 5)",
                rusqlite::params![
                    format!("dpr_{i}"),
                    format!("{i:064}"),
                    format!("U{i}"),
                    T0,
                    T0 + 10 * MINUTE
                ],
            )
            .expect("fill");
    }
    let full = block_on(service::start(&db, BASE, None, T0 + 1)).expect_err("full");
    assert_eq!(full.status, 429);
    assert_eq!(limits::retry_after(&full), Some(600));
    block_on(service::start(&db, BASE, None, T0 + 10 * MINUTE)).expect("expired ones free the cap");
}

#[test]
fn purge_removes_long_expired_pairings_and_old_counters_and_start_does_it_too() {
    let db = Fake::new();
    let owner = owner(&db);
    let (old_code, _) = started(&db, T0);
    miss(&db, &owner, T0);
    miss(&db, &owner, T0);
    // Expired but within the grace hour: still answers expired_token.
    let control = FakeControl::default();
    assert_eq!(
        rfc(&poll(&db, &control, &old_code, T0 + 30 * MINUTE).expect_err("expired")),
        "expired_token"
    );
    assert_eq!(
        block_on(limits::purge(&db, T0 + 30 * MINUTE)).expect("purge"),
        (0, 0)
    );
    assert_eq!(db.rows("SELECT id FROM device_pairings").len(), 1);

    // Past the grace hour (and the longest counter window) they are gone.
    let later = T0 + 10 * MINUTE + 61 * MINUTE;
    assert_eq!(block_on(limits::purge(&db, later)).expect("purge"), (1, 4));
    assert!(db.rows("SELECT id FROM device_pairings").is_empty());
    assert!(db.rows("SELECT scope FROM device_attempts").is_empty());

    // Opportunistic: start purges as it goes.
    started(&db, later);
    miss(&db, &owner, later);
    let much_later = later + 3 * 60 * MINUTE;
    started(&db, much_later);
    assert_eq!(db.rows("SELECT id FROM device_pairings").len(), 1);
    assert!(db.rows("SELECT scope FROM device_attempts").is_empty());
}

#[test]
fn retry_after_reads_only_rate_limit_errors() {
    let limited = limits::Limited {
        retry_after_seconds: 42,
    }
    .into_api();
    assert_eq!((limited.status, limited.code), (429, "rate_limited"));
    assert_eq!(limits::retry_after(&limited), Some(42));
    assert_eq!(
        limits::retry_after(&ApiError::not_found(
            "too many attempts; retry in 3 seconds"
        )),
        None
    );
}

#[test]
fn the_token_names_the_approving_org_and_user() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0).expect("approve");
    let granted = poll(&db, &control, &device_code, T0 + 6_000).expect("granted");
    let stored = db.rows(
        "SELECT o.name AS org_name, u.display_name AS user_name, u.email AS user_email \
         FROM orgs o, users u WHERE o.id = 'org_dev' AND u.id = 'usr_dev'",
    );
    assert_eq!(granted["org"]["id"], owner.org_id.as_str());
    assert_eq!(granted["org"]["name"], stored[0]["org_name"]);
    assert!(
        !granted["org"]["name"]
            .as_str()
            .expect("org name")
            .is_empty()
    );
    assert_eq!(granted["approved_by"]["id"], owner.user_id.as_str());
    assert_eq!(granted["approved_by"]["name"], stored[0]["user_name"]);
    assert_eq!(
        granted["approved_by"]["email"].as_str().unwrap_or_default(),
        stored[0]["user_email"].as_str().unwrap_or_default()
    );
}

#[test]
fn a_machine_revokes_itself_with_its_own_credential_only() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (device_code, user_code) = started(&db, T0);
    approve(&db, &owner, &user_code, T0).expect("approve");
    let granted = poll(&db, &control, &device_code, T0 + 6_000).expect("granted");
    let machine_id = granted["machine_id"].as_str().expect("id").to_owned();
    let bearer = format!(
        "Bearer {}",
        granted["credential"].as_str().expect("credential")
    );
    let call = |authorization: Option<&str>| {
        block_on(service::revoke_self(&db, &control, authorization, "r_self"))
    };

    assert_eq!(call(None).expect_err("no credential").status, 401);
    assert_eq!(
        call(Some("Bearer smm_wrong")).expect_err("wrong").status,
        401
    );
    // An admin-API machine's credential is not a paired device's.
    let admin_machine = block_on(crate::machines::admin::create_machine(
        &db,
        &control,
        &owner,
        &json!({ "name": "ci" }),
        "r",
    ))
    .expect("machine");
    let admin_bearer = format!(
        "Bearer {}",
        admin_machine["credential"].as_str().expect("credential")
    );
    assert_eq!(
        call(Some(&admin_bearer)).expect_err("admin machine").status,
        401
    );
    assert!(control.revoked.borrow().is_empty());

    call(Some(&bearer)).expect("revoked");
    assert_eq!(
        control.revoked.borrow().as_slice(),
        std::slice::from_ref(&machine_id)
    );
    assert_eq!(verdict(&db, &machine_id, false), trust::Verdict::Revoked);
    let audit = db.rows("SELECT action FROM audit_log ORDER BY seq");
    assert_eq!(audit.last().expect("audit")["action"], "device.decline");
    // The credential no longer works for anything.
    assert_eq!(call(Some(&bearer)).expect_err("again").status, 401);
}
