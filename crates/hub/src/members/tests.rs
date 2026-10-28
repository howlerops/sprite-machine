//! Native tests of the members and invites routes over the real migrations (the HTTP surface,
//! with a real GitHub sign-in against a mock GitHub, is `tests/oauth.rs`).

use serde_json::{Value, json};

use super::{gate, handle};
use crate::auth::Principal;
use crate::auth::github_tests::{Fake, T0, block_on, seed_org};
use crate::auth::service::issue_session;
use crate::threads::ApiError;

/// A session of `role` in the owner's org; returns its principal and `Authorization` value.
fn session(db: &Fake, owner: &Principal, role: &str, name: &str) -> (Principal, String) {
    let body = json!({ "role": role, "display_name": name });
    let issued = block_on(issue_session(db, owner, &body, "req_s")).expect("issue");
    let bearer = format!(
        "Bearer {}",
        issued["session"]["token"].as_str().expect("token")
    );
    let principal = block_on(crate::auth::authenticate(db, Some(&bearer), false)).expect("auth");
    (principal, bearer)
}

/// Runs a route as the holder of `bearer`: the gate, then the handler.
fn call(
    db: &Fake,
    bearer: Option<&str>,
    method: &str,
    path: &str,
    body: Value,
    now: i64,
) -> Result<(u16, Value), ApiError> {
    block_on(async {
        let (caller, policy) = gate(db, bearer, false, method, path).await?;
        handle(db, &caller, &policy, method, &body, now, "req_m").await
    })
}

fn invite_body(login: &str, role: Option<&str>) -> Value {
    match role {
        Some(role) => json!({ "github_login": login, "role": role }),
        None => json!({ "github_login": login }),
    }
}

#[test]
fn membership_is_required_and_viewers_read_but_only_admins_manage() {
    let db = Fake::new();
    let owner = seed_org(&db, "org_a", "usr_owner");
    let (_, viewer) = session(&db, &owner, "viewer", "vic");
    let (_, member) = session(&db, &owner, "member", "mia");

    // No credential, or one that is not a session: 401, nothing leaks.
    for bearer in [None, Some("Bearer smt_nope"), Some("Bearer garbage")] {
        let error = call(&db, bearer, "GET", "/v1/members", json!({}), T0).expect_err("401");
        assert_eq!(error.status, 401);
    }
    let (status, body) =
        call(&db, Some(&viewer), "GET", "/v1/members", json!({}), T0).expect("viewer reads");
    assert_eq!(status, 200);
    assert_eq!(body["members"].as_array().unwrap().len(), 3);
    for bearer in [&viewer, &member] {
        for (method, path) in [
            ("GET", "/v1/invites"),
            ("POST", "/v1/invites"),
            ("DELETE", "/v1/invites/inv_1"),
            ("PATCH", "/v1/members/usr_owner"),
            ("DELETE", "/v1/members/usr_owner"),
        ] {
            let error = call(
                &db,
                Some(bearer),
                method,
                path,
                invite_body("octocat", None),
                T0,
            )
            .expect_err("403");
            assert_eq!(error.status, 403, "{method} {path}");
        }
    }
    assert_eq!(
        call(&db, Some(&viewer), "PUT", "/v1/members", json!({}), T0)
            .unwrap_err()
            .status,
        404
    );
    assert_eq!(db.count("SELECT COUNT(*) AS n FROM org_invites"), 0);
}

#[test]
fn an_admin_invites_a_github_login_with_a_role_no_higher_than_their_own() {
    let db = Fake::new();
    let owner = seed_org(&db, "org_a", "usr_owner");
    let _ = owner;
    db.exec("INSERT INTO users (id, display_name) VALUES ('usr_adm', 'adm');
             INSERT INTO members (org_id, user_id, role) VALUES ('org_a', 'usr_adm', 'admin');
             INSERT INTO devices (id, org_id, user_id, name) VALUES ('dev_adm', 'org_a', 'usr_adm', 'cli')");
    let (_, adm) = admin_session(&db);

    let (status, body) = call(
        &db,
        Some(&adm),
        "POST",
        "/v1/invites",
        invite_body("Octo-Cat", None),
        T0,
    )
    .expect("invite");
    assert_eq!(status, 201);
    assert_eq!(body["invite"]["github_login"], json!("octo-cat"));
    assert_eq!(body["invite"]["role"], json!("member"));
    let id = body["invite"]["id"].as_str().unwrap().to_owned();

    // Admin may invite an admin, not an owner.
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "POST",
            "/v1/invites",
            invite_body("dana", Some("admin")),
            T0
        )
        .unwrap()
        .0,
        201
    );
    let error = call(
        &db,
        Some(&adm),
        "POST",
        "/v1/invites",
        invite_body("erin", Some("owner")),
        T0,
    )
    .unwrap_err();
    assert_eq!(error.status, 403);
    // Bad logins and roles are 400; a duplicate open invite is 409.
    for bad in ["", "-x", "a b", "x-", &"a".repeat(40)] {
        assert_eq!(
            call(
                &db,
                Some(&adm),
                "POST",
                "/v1/invites",
                invite_body(bad, None),
                T0
            )
            .unwrap_err()
            .status,
            400,
            "{bad:?}"
        );
    }
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "POST",
            "/v1/invites",
            invite_body("frank", Some("god")),
            T0
        )
        .unwrap_err()
        .status,
        400
    );
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "POST",
            "/v1/invites",
            invite_body("OCTO-cat", None),
            T0
        )
        .unwrap_err()
        .status,
        409
    );

    // Listed, then revoked, then invitable again.
    let (_, listed) = call(&db, Some(&adm), "GET", "/v1/invites", json!({}), T0).expect("list");
    assert_eq!(listed["invites"].as_array().unwrap().len(), 2);
    let path = format!("/v1/invites/{id}");
    assert_eq!(
        call(&db, Some(&adm), "DELETE", &path, json!({}), T0 + 1)
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        call(&db, Some(&adm), "DELETE", &path, json!({}), T0 + 2)
            .unwrap_err()
            .status,
        404
    );
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "POST",
            "/v1/invites",
            invite_body("octo-cat", None),
            T0 + 3
        )
        .unwrap()
        .0,
        201
    );

    // Expired invites are not listed.
    let (_, later) = call(
        &db,
        Some(&adm),
        "GET",
        "/v1/invites",
        json!({}),
        T0 + crate::auth::github::INVITE_TTL_MS + 10,
    )
    .unwrap();
    assert!(later["invites"].as_array().unwrap().is_empty());

    // Every mutation is audited with the actor from the session.
    let actions: Vec<String> = db
        .rows("SELECT action FROM audit_log WHERE action LIKE 'member.%' ORDER BY seq")
        .iter()
        .map(|r| r["action"].as_str().unwrap().to_owned())
        .collect();
    assert_eq!(
        actions,
        [
            "member.invite",
            "member.invite",
            "member.invite.revoke",
            "member.invite"
        ]
    );
}

#[test]
fn inviting_an_existing_member_is_a_conflict_and_invites_stay_in_their_org() {
    let db = Fake::new();
    let owner_a = seed_org(&db, "org_a", "usr_a");
    let owner_b = seed_org(&db, "org_b", "usr_b");
    db.exec("UPDATE users SET github_login = 'Ann' WHERE id = 'usr_a'");
    let (_, a) = session(&db, &owner_a, "owner", "a2");
    let (_, b) = session(&db, &owner_b, "owner", "b2");

    assert_eq!(
        call(
            &db,
            Some(&a),
            "POST",
            "/v1/invites",
            invite_body("ann", None),
            T0
        )
        .unwrap_err()
        .status,
        409
    );
    // The same login may be invited to a different org.
    assert_eq!(
        call(
            &db,
            Some(&b),
            "POST",
            "/v1/invites",
            invite_body("ann", None),
            T0
        )
        .unwrap()
        .0,
        201
    );
    let id = db.rows("SELECT id FROM org_invites")[0]["id"]
        .as_str()
        .unwrap()
        .to_owned();
    // Org A's admin cannot see or revoke org B's invite, and sees none of B's members.
    assert_eq!(
        call(
            &db,
            Some(&a),
            "DELETE",
            &format!("/v1/invites/{id}"),
            json!({}),
            T0
        )
        .unwrap_err()
        .status,
        404
    );
    let (_, listed) = call(&db, Some(&a), "GET", "/v1/invites", json!({}), T0).unwrap();
    assert!(listed["invites"].as_array().unwrap().is_empty());
    let (_, members) = call(&db, Some(&a), "GET", "/v1/members", json!({}), T0).unwrap();
    assert!(
        members["members"]
            .as_array()
            .unwrap()
            .iter()
            .all(|m| m["user_id"] != json!("usr_b"))
    );
    assert_eq!(
        call(
            &db,
            Some(&a),
            "PATCH",
            "/v1/members/usr_b",
            json!({"role": "viewer"}),
            T0
        )
        .unwrap_err()
        .status,
        404
    );
    assert_eq!(
        call(&db, Some(&a), "DELETE", "/v1/members/usr_b", json!({}), T0)
            .unwrap_err()
            .status,
        404
    );
}

#[test]
fn roles_change_within_the_callers_rank_and_a_removed_member_loses_the_session() {
    let db = Fake::new();
    let owner = seed_org(&db, "org_a", "usr_owner");
    let (_, boss) = session(&db, &owner, "owner", "boss");
    let (member_p, member) = session(&db, &owner, "member", "mia");
    let (viewer_p, _) = session(&db, &owner, "viewer", "vic");

    // Promote and demote; an unknown role is 400; yourself is 403.
    let path = format!("/v1/members/{}", viewer_p.user_id);
    assert_eq!(
        call(
            &db,
            Some(&boss),
            "PATCH",
            &path,
            json!({"role": "member"}),
            T0
        )
        .unwrap()
        .0,
        200
    );
    assert_eq!(
        call(
            &db,
            Some(&boss),
            "PATCH",
            &path,
            json!({"role": "wizard"}),
            T0
        )
        .unwrap_err()
        .status,
        400
    );
    assert_eq!(
        call(&db, Some(&boss), "PATCH", &path, json!({}), T0)
            .unwrap_err()
            .status,
        400
    );
    let own = format!(
        "/v1/members/{}",
        block_on(crate::auth::authenticate(&db, Some(&boss), false))
            .unwrap()
            .user_id
    );
    assert_eq!(
        call(
            &db,
            Some(&boss),
            "PATCH",
            &own,
            json!({"role": "viewer"}),
            T0
        )
        .unwrap_err()
        .status,
        403
    );
    assert_eq!(
        call(&db, Some(&boss), "DELETE", &own, json!({}), T0)
            .unwrap_err()
            .status,
        403
    );

    // The new role applies to the live session at once.
    let (_, listed) = call(&db, Some(&boss), "GET", "/v1/members", json!({}), T0).unwrap();
    let roles: Vec<&str> = listed["members"]
        .as_array()
        .unwrap()
        .iter()
        .map(|m| m["role"].as_str().unwrap())
        .collect();
    assert_eq!(roles.iter().filter(|r| **r == "member").count(), 2);

    // Remove the member: their token stops working (401) and the session row is marked revoked.
    let removed = format!("/v1/members/{}", member_p.user_id);
    assert_eq!(
        call(&db, Some(&boss), "DELETE", &removed, json!({}), T0)
            .unwrap()
            .0,
        200
    );
    assert_eq!(
        call(&db, Some(&member), "GET", "/v1/members", json!({}), T0)
            .unwrap_err()
            .status,
        401
    );
    assert_eq!(
        db.count(&format!(
            "SELECT COUNT(*) AS n FROM sessions WHERE user_id = '{}' AND revoked_at IS NULL",
            member_p.user_id
        )),
        0
    );
    assert_eq!(
        call(&db, Some(&boss), "DELETE", &removed, json!({}), T0)
            .unwrap_err()
            .status,
        404
    );
}

#[test]
fn an_admin_cannot_touch_an_owner_or_grant_above_their_own_role() {
    let db = Fake::new();
    let owner = seed_org(&db, "org_a", "usr_owner");
    db.exec("INSERT INTO users (id, display_name) VALUES ('usr_adm', 'adm');
             INSERT INTO members (org_id, user_id, role) VALUES ('org_a', 'usr_adm', 'admin');
             INSERT INTO devices (id, org_id, user_id, name) VALUES ('dev_adm', 'org_a', 'usr_adm', 'cli')");
    let (_, adm) = admin_session(&db);
    let (viewer_p, _) = session(&db, &owner, "viewer", "vic");

    assert_eq!(
        call(
            &db,
            Some(&adm),
            "PATCH",
            "/v1/members/usr_owner",
            json!({"role": "viewer"}),
            T0
        )
        .unwrap_err()
        .status,
        403
    );
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "DELETE",
            "/v1/members/usr_owner",
            json!({}),
            T0
        )
        .unwrap_err()
        .status,
        403
    );
    let path = format!("/v1/members/{}", viewer_p.user_id);
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "PATCH",
            &path,
            json!({"role": "owner"}),
            T0
        )
        .unwrap_err()
        .status,
        403
    );
    assert_eq!(
        call(
            &db,
            Some(&adm),
            "PATCH",
            &path,
            json!({"role": "admin"}),
            T0
        )
        .unwrap()
        .0,
        200
    );
    assert_eq!(
        db.count("SELECT COUNT(*) AS n FROM members WHERE role = 'owner'"),
        1
    );
}

/// A real session for the seeded admin `usr_adm` (inserted directly: `issue_session` never mints
/// an admin for a non-owner).
fn admin_session(db: &Fake) -> (Principal, String) {
    use crate::auth::token::{hash_token, new_token};
    let token = new_token().expect("token");
    db.exec(&format!(
        "INSERT INTO sessions (id, token_hash, user_id, device_id, org_id, expires_at) \
         VALUES ('ses_adm', '{}', 'usr_adm', 'dev_adm', 'org_a', '2999-01-01T00:00:00.000Z')",
        hash_token(&token)
    ));
    let bearer = format!("Bearer {token}");
    let principal = block_on(crate::auth::authenticate(db, Some(&bearer), false)).expect("auth");
    (principal, bearer)
}
