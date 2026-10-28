//! Native tests of browser sign-in (M2-003c): the sign-in link a paired machine mints and the
//! page redeems, and the browser device-code flow. Same fake database and clock-in-hand style as
//! the pairing tests in the parent module; the live tests are `tests/signin.rs`.

use serde_json::{Value, json};

use super::{BASE, Fake, FakeControl, MINUTE, T0};
use super::{approve, block_on, miss, owner, poll, rfc, started, with_role};
use crate::auth::{Principal, authenticate};
use crate::db::Role;
use crate::devices::service::PollError;
use crate::devices::{browser, limits, signin};
use crate::machines::credential::hash;
use crate::threads::ApiError;

/// Pairs a machine through the device flow; returns its `Bearer` header and id.
fn paired_machine(db: &Fake, control: &FakeControl, owner: &Principal) -> (String, String) {
    let (device_code, user_code) = started(db, T0);
    approve(db, owner, &user_code, T0).expect("approve");
    let granted = poll(db, control, &device_code, T0 + 6_000).expect("granted");
    (
        format!(
            "Bearer {}",
            granted["credential"].as_str().expect("credential")
        ),
        granted["machine_id"]
            .as_str()
            .expect("machine id")
            .to_owned(),
    )
}

fn mint(db: &Fake, bearer: &str, now: i64) -> Result<Value, ApiError> {
    block_on(signin::mint(db, Some(bearer), BASE, now, "req_mint"))
}

fn minted_code(db: &Fake, bearer: &str, now: i64) -> String {
    let minted = mint(db, bearer, now).expect("mint");
    let url = minted["url"].as_str().expect("url");
    url.split_once("/pair?code=")
        .expect("pair url")
        .1
        .to_owned()
}

fn redeem(db: &Fake, code: &str, now: i64) -> Result<Value, ApiError> {
    block_on(signin::redeem(
        db,
        &json!({ "code": code }),
        now,
        "req_redeem",
    ))
}

#[test]
fn a_paired_machine_mints_a_link_that_redeems_once_into_a_capped_session() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (bearer, machine_id) = paired_machine(&db, &control, &owner);

    let minted = mint(&db, &bearer, T0 + 10_000).expect("mint");
    let url = minted["url"].as_str().expect("url");
    assert!(url.starts_with("https://hub.example/pair?code=sli_"));
    let code = minted_code(&db, &bearer, T0 + 10_000);
    assert_eq!(code.len(), 4 + 64);
    assert_eq!(minted["expires_at"], signin::iso(T0 + 10_000 + 5 * MINUTE));
    // Only the hash is stored, bound to the approving user, the org and the machine.
    let stored = db.rows("SELECT code_hash, org_id, user_id, machine_id FROM signin_links");
    assert_eq!(stored.len(), 2);
    assert_eq!(stored[1]["code_hash"], hash(&code));
    assert!(!format!("{stored:?}").contains(&code));
    assert_eq!(stored[1]["user_id"], owner.user_id.as_str());
    assert_eq!(stored[1]["org_id"], owner.org_id.as_str());
    assert_eq!(stored[1]["machine_id"], machine_id.as_str());

    let session = redeem(&db, &code, T0 + 20_000).expect("redeem");
    let token = session["token"].as_str().expect("token");
    assert!(token.starts_with("smt_"));
    assert_eq!(session["user"]["id"], owner.user_id.as_str());
    assert_eq!(session["org"]["id"], owner.org_id.as_str());
    // The dev user is an owner; the link's session is capped at member, on a "web" device.
    assert_eq!(session["role"], "member");
    let who = block_on(authenticate(&db, Some(&format!("Bearer {token}")), false))
        .expect("the session works");
    assert_eq!(who.user_id, owner.user_id);
    assert_eq!(who.org_id, owner.org_id);
    assert_eq!(who.role, Role::Member);
    assert!(who.require(Role::Admin).is_err());
    let device = db.rows(&format!(
        "SELECT name FROM devices WHERE id = '{}'",
        who.device_id
    ));
    assert_eq!(device[0]["name"], "web");

    // The audit row names the session, never the code.
    let audit = db.rows("SELECT action FROM audit_log WHERE action LIKE 'session.%'");
    assert_eq!(audit.len(), 1);
    assert_eq!(audit[0]["action"], "session.link_redeem");
    assert!(!format!("{:?}", db.rows("SELECT * FROM audit_log")).contains(&code));

    // Single use; unknown, malformed and expired codes give the same 410.
    for code in [
        code.clone(),
        format!("sli_{}", "0".repeat(64)),
        "nonsense".to_owned(),
    ] {
        let error = redeem(&db, &code, T0 + 30_000).expect_err("refused");
        assert_eq!((error.status, error.code), (410, "expired"));
    }
}

#[test]
fn a_link_expires_after_five_minutes_on_the_injected_clock() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (bearer, _) = paired_machine(&db, &control, &owner);
    let t = T0 + 10_000;
    let late = minted_code(&db, &bearer, t);
    let error = redeem(&db, &late, t + 5 * MINUTE).expect_err("expired");
    assert_eq!((error.status, error.code), (410, "expired"));
    let in_time = minted_code(&db, &bearer, t);
    redeem(&db, &in_time, t + 5 * MINUTE - 1).expect("redeems just before expiry");
}

#[test]
fn only_a_live_device_origin_machine_may_mint() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    // The admin API's machine has no approving user.
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
    let error = mint(&db, &admin_bearer, T0).expect_err("admin origin");
    assert_eq!((error.status, error.code), (403, "forbidden"));

    for bad in [None, Some("Bearer smm_wrong"), Some("Bearer smt_wrong")] {
        let error = block_on(signin::mint(&db, bad, BASE, T0, "r")).expect_err("unknown");
        assert_eq!(error.status, 401);
    }

    let (bearer, machine_id) = paired_machine(&db, &control, &owner);
    let code = minted_code(&db, &bearer, T0 + 10_000);
    // Revoked: no new links (401), and links it already minted stop redeeming (410).
    block_on(crate::machines::admin::revoke_machine(
        &db,
        &control,
        &owner,
        &machine_id,
        "r",
    ))
    .expect("revoke");
    assert_eq!(
        mint(&db, &bearer, T0 + 11_000).expect_err("revoked").status,
        401
    );
    assert_eq!(
        redeem(&db, &code, T0 + 12_000).expect_err("revoked").status,
        410
    );
}

#[test]
fn a_machine_holds_a_bounded_number_of_live_links() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (bearer, _) = paired_machine(&db, &control, &owner);
    for _ in 0..signin::MAX_LIVE_PER_MACHINE {
        mint(&db, &bearer, T0 + 10_000).expect("mint");
    }
    let limited = mint(&db, &bearer, T0 + 10_001).expect_err("limited");
    assert_eq!(limited.status, 429);
    mint(&db, &bearer, T0 + 10_000 + 5 * MINUTE).expect("the old links expired");
}

#[test]
fn a_removed_member_cannot_redeem_or_mint() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (bearer, _) = paired_machine(&db, &control, &owner);
    let code = minted_code(&db, &bearer, T0 + 10_000);
    db.rows("DELETE FROM members WHERE user_id = 'usr_dev' RETURNING user_id");
    assert_eq!(
        redeem(&db, &code, T0 + 11_000).expect_err("gone").status,
        410
    );
    assert_eq!(
        mint(&db, &bearer, T0 + 12_000).expect_err("gone").status,
        403
    );
}

#[test]
fn iso_formats_epoch_milliseconds() {
    assert_eq!(signin::iso(0), "1970-01-01T00:00:00.000Z");
    assert_eq!(signin::iso(1_709_210_096_789), "2024-02-29T12:34:56.789Z");
}

fn browser_start(db: &Fake, now: i64) -> (String, String) {
    let out = block_on(browser::start(db, BASE, None, now)).expect("start");
    (
        out["device_code"].as_str().expect("device_code").to_owned(),
        out["user_code"].as_str().expect("user_code").to_owned(),
    )
}

fn browser_approve(
    db: &Fake,
    who: &Principal,
    user_code: &str,
    now: i64,
) -> Result<Value, ApiError> {
    block_on(browser::approve(
        db,
        who,
        &json!({ "user_code": user_code }),
        now,
        "req_b",
    ))
}

fn browser_poll(db: &Fake, code: &str, now: i64) -> Result<Value, PollError> {
    block_on(browser::poll(db, code, now, "req_p"))
}

#[test]
fn the_browser_flow_hands_out_one_capped_session_naming_user_and_org() {
    let db = Fake::new();
    let owner = owner(&db);
    let started_out =
        block_on(browser::start(&db, "https://hub.example/", None, T0)).expect("start");
    assert_eq!(started_out["expires_in"], 600);
    assert_eq!(started_out["interval"], 5);
    let device_code = started_out["device_code"]
        .as_str()
        .expect("device code")
        .to_owned();
    let user_code = started_out["user_code"]
        .as_str()
        .expect("user code")
        .to_owned();
    assert_eq!(user_code.len(), 9);
    assert!(
        started_out["verification_uri"]
            .as_str()
            .expect("uri")
            .starts_with("https://hub.example/")
    );
    let stored = db.rows("SELECT device_code_hash FROM browser_pairings");
    assert_eq!(stored[0]["device_code_hash"], hash(&device_code));

    assert_eq!(
        rfc(&browser_poll(&db, &device_code, T0).expect_err("pending")),
        "authorization_pending"
    );
    browser_approve(&db, &owner, &user_code.to_lowercase(), T0 + 6_000).expect("approve");
    let granted = browser_poll(&db, &device_code, T0 + 12_000).expect("granted");
    let token = granted["token"].as_str().expect("token");
    assert_eq!(granted["user"]["id"], owner.user_id.as_str());
    assert_eq!(granted["org"]["id"], owner.org_id.as_str());
    assert_eq!(granted["role"], "member");
    let who =
        block_on(authenticate(&db, Some(&format!("Bearer {token}")), false)).expect("session");
    assert_eq!(
        (who.user_id.as_str(), who.role),
        (owner.user_id.as_str(), Role::Member)
    );
    let audit = db.rows("SELECT action FROM audit_log ORDER BY seq");
    let actions: Vec<_> = audit.iter().map(|r| r["action"].clone()).collect();
    assert_eq!(
        actions,
        [json!("auth.device.approve"), json!("auth.device.grant")]
    );
    assert!(!format!("{:?}", db.rows("SELECT * FROM audit_log")).contains(&device_code));

    // Once: then invalid_grant, and no second session.
    assert_eq!(
        rfc(&browser_poll(&db, &device_code, T0 + 30_000).expect_err("used")),
        "invalid_grant"
    );
    assert_eq!(db.rows("SELECT id FROM sessions").len(), 1);

    // A machine pairing's codes are not browser codes.
    let (machine_code, machine_user_code) = started(&db, T0 + 35_000);
    assert_eq!(
        rfc(&browser_poll(&db, &machine_code, T0 + 40_000).expect_err("other table")),
        "invalid_grant"
    );
    assert_eq!(
        browser_approve(&db, &owner, &machine_user_code, T0 + 40_000)
            .expect_err("other table")
            .status,
        404
    );
}

#[test]
fn browser_polling_errors_follow_rfc_8628() {
    let db = Fake::new();
    let owner = owner(&db);
    assert_eq!(
        rfc(&browser_poll(&db, "sdc_unknown", T0).expect_err("unknown")),
        "invalid_grant"
    );
    let (device_code, user_code) = browser_start(&db, T0);
    assert_eq!(
        rfc(&browser_poll(&db, &device_code, T0).expect_err("first")),
        "authorization_pending"
    );
    assert_eq!(
        browser_poll(&db, &device_code, T0 + 1_000).expect_err("too fast"),
        PollError::Rfc {
            code: "slow_down",
            interval: Some(10)
        }
    );
    // Expired after ten minutes, even if approved in time.
    browser_approve(&db, &owner, &user_code, T0 + 9 * MINUTE).expect("approve in time");
    assert_eq!(
        rfc(&browser_poll(&db, &device_code, T0 + 10 * MINUTE).expect_err("expired")),
        "expired_token"
    );
    assert!(db.rows("SELECT id FROM sessions").is_empty());
    // Approving an expired code is refused.
    let (_, user_code) = browser_start(&db, T0);
    assert_eq!(
        browser_approve(&db, &owner, &user_code, T0 + 10 * MINUTE)
            .expect_err("expired")
            .status,
        404
    );
}

#[test]
fn browser_approval_needs_a_member_and_shares_the_guessing_limit() {
    let db = Fake::new();
    let owner = owner(&db);
    let (_, user_code) = browser_start(&db, T0);
    let viewer = with_role(&owner, Role::Viewer);
    assert_eq!(
        browser_approve(&db, &viewer, &user_code, T0)
            .expect_err("viewer")
            .status,
        403
    );
    assert_eq!(
        browser_approve(&db, &owner, "not a code", T0)
            .expect_err("bad")
            .status,
        400
    );
    for i in 0..5 {
        assert_eq!(
            browser_approve(&db, &owner, "ABCD-EFGH", T0 + i)
                .expect_err("miss")
                .status,
            404
        );
    }
    let limited = browser_approve(&db, &owner, &user_code, T0 + 10).expect_err("limited");
    assert_eq!(limited.status, 429);
    assert_eq!(limits::retry_after(&limited), Some(60));
    browser_approve(&db, &owner, &user_code, T0 + MINUTE + 1).expect("allowed again");

    // The machine-pairing counters are the same ones: misses there limit the browser flow too.
    let db = Fake::new();
    let owner = super::owner(&db);
    for i in 0..5 {
        assert_eq!(miss(&db, &owner, T0 + i), 404);
    }
    let (_, user_code) = browser_start(&db, T0);
    assert_eq!(
        browser_approve(&db, &owner, &user_code, T0 + 10)
            .expect_err("shared")
            .status,
        429
    );
}

#[test]
fn starting_a_browser_pairing_is_limited_per_ip() {
    let db = Fake::new();
    for i in 0..limits::CODE_PER_IP_PER_MINUTE {
        block_on(browser::start(&db, BASE, Some("203.0.113.9"), T0 + i)).expect("start");
    }
    let limited =
        block_on(browser::start(&db, BASE, Some("203.0.113.9"), T0 + 100)).expect_err("limited");
    assert_eq!(limited.status, 429);
    block_on(browser::start(&db, BASE, Some("203.0.113.10"), T0 + 100)).expect("another ip");
    block_on(browser::start(
        &db,
        BASE,
        Some("203.0.113.9"),
        T0 + MINUTE + 100,
    ))
    .expect("window moved");
}

#[test]
fn purge_drops_old_links_and_browser_pairings() {
    let db = Fake::new();
    let control = FakeControl::default();
    let owner = owner(&db);
    let (bearer, _) = paired_machine(&db, &control, &owner);
    mint(&db, &bearer, T0).expect("mint");
    browser_start(&db, T0);
    block_on(limits::purge(&db, T0 + 30 * MINUTE)).expect("purge");
    assert_eq!(
        db.rows("SELECT id FROM signin_links").len(),
        1,
        "within the grace period"
    );
    block_on(limits::purge(&db, T0 + 120 * MINUTE)).expect("purge");
    assert!(db.rows("SELECT id FROM signin_links").is_empty());
    assert!(db.rows("SELECT id FROM browser_pairings").is_empty());
}
