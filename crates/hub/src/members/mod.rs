//! Org members and invites (M2-003b, plan §3 Settings "members", §4.2 "Auth").
//!
//! - `GET /v1/members` (viewer): the caller's org members.
//! - `POST /v1/invites {github_login, role?}` (admin): invite a GitHub login. The role may not
//!   exceed the inviter's own. The invite is accepted by the first GitHub sign-in of that account
//!   (`auth::github`), which makes them a member.
//! - `GET /v1/invites` (admin), `DELETE /v1/invites/{id}` (admin): open invites, revoke one.
//! - `PATCH /v1/members/{user_id} {role}`, `DELETE /v1/members/{user_id}` (admin): change a role
//!   or remove a member. Never above your own role, never yourself (so an org keeps an owner),
//!   and a removed member's sessions in the org are revoked at once.
//!
//! Every route is scoped to the org of the caller's session. Someone who is not a member has no
//! session for the org (`401`), a member of another org cannot name this org's rows (`404`), and a
//! role below the route's is `403` ([`crate::auth::policy::member_policy`]). Each mutation writes
//! its audit row before it changes anything. `routes` (wasm32 only) is the axum glue.

use serde_json::{Value, json};

use crate::audit::{self, Event};
use crate::auth::github::{INVITE_TTL_MS, is_valid_login};
use crate::auth::policy::{OrgPolicy, member_policy};
use crate::auth::token::new_id;
use crate::auth::{Principal, authenticate};
use crate::db::{Db, DbError, Role, Row, int, opt_text, text};
use crate::threads::ApiError;

#[cfg(target_arch = "wasm32")]
pub mod routes;

#[cfg(test)]
mod tests;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("members: {error}"))
}

fn forbidden(message: String) -> ApiError {
    ApiError::new(403, "forbidden", message)
}

fn conflict(message: &str) -> ApiError {
    ApiError::new(409, "conflict", message)
}

fn parse_role(value: Option<&Value>, default: Option<Role>) -> Result<Role, ApiError> {
    match value {
        None | Some(Value::Null) => {
            default.ok_or_else(|| ApiError::invalid_request("role is required"))
        }
        Some(Value::String(role)) => role
            .parse::<Role>()
            .map_err(|error| ApiError::invalid_request(error.to_string())),
        Some(_) => Err(ApiError::invalid_request("role must be a string")),
    }
}

/// A role the caller may hand out or touch: never above their own.
fn require_not_above(caller: &Principal, role: Role, what: &str) -> Result<(), ApiError> {
    if role.rank() > caller.role.rank() {
        Err(forbidden(format!(
            "a {} cannot {what} a {role}",
            caller.role
        )))
    } else {
        Ok(())
    }
}

/// Authenticates and applies the route's role gate. `Bearer` session (or `dev` with
/// `DEV_FIXTURES`), then [`member_policy`]: an unknown route is `404`, too low a role `403`.
pub async fn gate(
    db: &impl Db,
    authorization: Option<&str>,
    dev_fixtures: bool,
    method: &str,
    path: &str,
) -> Result<(Principal, OrgPolicy), ApiError> {
    let principal = authenticate(db, authorization, dev_fixtures).await?;
    let policy = member_policy(method, path)
        .ok_or_else(|| ApiError::not_found(format!("no route for {method} {path}")))?;
    principal.require(policy.role)?;
    Ok((principal, policy))
}

/// Runs one gated members route; returns the success status and body.
pub async fn handle(
    db: &impl Db,
    caller: &Principal,
    policy: &OrgPolicy,
    method: &str,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<(u16, Value), ApiError> {
    let target = policy.target.as_deref().unwrap_or_default();
    match (method, policy.action) {
        ("GET", None) if policy.role == Role::Viewer => Ok((200, list_members(db, caller).await?)),
        ("GET", None) => Ok((200, list_invites(db, caller, now_ms).await?)),
        ("POST", Some(action)) => Ok((
            201,
            create_invite(db, caller, action, body, now_ms, request_id).await?,
        )),
        ("DELETE", Some("member.invite.revoke")) => Ok((
            200,
            revoke_invite(db, caller, target, now_ms, request_id).await?,
        )),
        ("PATCH", Some(action)) => Ok((
            200,
            set_role(db, caller, action, target, body, request_id).await?,
        )),
        ("DELETE", Some(action)) => Ok((
            200,
            remove_member(db, caller, action, target, request_id).await?,
        )),
        _ => Err(ApiError::not_found(format!("no route for {method}"))),
    }
}

fn member_json(row: &Row) -> Result<Value, DbError> {
    Ok(json!({
        "user_id": text(row, "user_id")?,
        "display_name": text(row, "display_name")?,
        "github_login": opt_text(row, "github_login")?,
        "role": text(row, "role")?,
        "joined_at": text(row, "created_at")?,
    }))
}

pub async fn list_members(db: &impl Db, caller: &Principal) -> Result<Value, ApiError> {
    let rows = db
        .query(
            "SELECT m.user_id AS user_id, u.display_name AS display_name, \
             u.github_login AS github_login, m.role AS role, m.created_at AS created_at \
             FROM members m JOIN users u ON u.id = m.user_id WHERE m.org_id = ? \
             ORDER BY m.created_at, m.user_id",
            &[caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let members = rows
        .iter()
        .map(member_json)
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_failure)?;
    Ok(json!({ "members": members }))
}

fn invite_json(row: &Row) -> Result<Value, DbError> {
    Ok(json!({
        "id": text(row, "id")?,
        "github_login": text(row, "github_login")?,
        "role": text(row, "role")?,
        "invited_by": opt_text(row, "invited_by_user_id")?,
        "created_at_ms": int(row, "created_at_ms")?,
        "expires_at_ms": int(row, "expires_at_ms")?,
    }))
}

pub async fn list_invites(
    db: &impl Db,
    caller: &Principal,
    now_ms: i64,
) -> Result<Value, ApiError> {
    let rows = db
        .query(
            "SELECT id, github_login, role, invited_by_user_id, created_at_ms, expires_at_ms \
             FROM org_invites WHERE org_id = ? AND accepted_at_ms IS NULL \
               AND revoked_at_ms IS NULL AND expires_at_ms > ? ORDER BY created_at_ms, id",
            &[caller.org_id.as_str().into(), now_ms.into()],
        )
        .await
        .map_err(db_failure)?;
    let invites = rows
        .iter()
        .map(invite_json)
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_failure)?;
    Ok(json!({ "invites": invites }))
}

async fn open_invite_exists(db: &impl Db, org_id: &str, login: &str) -> Result<bool, ApiError> {
    Ok(!db
        .query(
            "SELECT id FROM org_invites WHERE org_id = ? AND github_login = ? \
             AND accepted_at_ms IS NULL AND revoked_at_ms IS NULL",
            &[org_id.into(), login.into()],
        )
        .await
        .map_err(db_failure)?
        .is_empty())
}

/// `POST /v1/invites`. The login is stored lowercase (GitHub logins are case-insensitive).
pub async fn create_invite(
    db: &impl Db,
    caller: &Principal,
    action: &str,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    let login = match body.get("github_login") {
        Some(Value::String(login)) if is_valid_login(login.trim()) => {
            login.trim().to_ascii_lowercase()
        }
        _ => {
            return Err(ApiError::invalid_request(
                "github_login must be a GitHub login (1-39 letters, digits or hyphens)",
            ));
        }
    };
    let role = parse_role(body.get("role"), Some(Role::Member))?;
    require_not_above(caller, role, "invite")?;

    let already_member = db
        .query(
            "SELECT m.user_id FROM members m JOIN users u ON u.id = m.user_id \
             WHERE m.org_id = ? AND lower(u.github_login) = ?",
            &[caller.org_id.as_str().into(), login.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    if !already_member.is_empty() {
        return Err(conflict("this GitHub account is already a member"));
    }
    if open_invite_exists(db, &caller.org_id, &login).await? {
        return Err(conflict("this GitHub account already has an open invite"));
    }

    let id = new_id("inv")?;
    audit::record(
        db,
        &Event::by(
            caller,
            action,
            "invite",
            &id,
            request_id,
            json!({ "github_login": login, "role": role.as_str() }),
        ),
    )
    .await?;
    let expires_at_ms = now_ms + INVITE_TTL_MS;
    let inserted = db
        .execute(
            "INSERT INTO org_invites (id, org_id, github_login, role, invited_by_user_id, \
             created_at_ms, expires_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
            &[
                id.as_str().into(),
                caller.org_id.as_str().into(),
                login.as_str().into(),
                role.as_str().into(),
                caller.user_id.as_str().into(),
                now_ms.into(),
                expires_at_ms.into(),
            ],
        )
        .await;
    if let Err(error) = inserted {
        // The open-invite index refused: a concurrent invite won.
        return Err(if open_invite_exists(db, &caller.org_id, &login).await? {
            conflict("this GitHub account already has an open invite")
        } else {
            db_failure(error)
        });
    }
    Ok(json!({
        "invite": {
            "id": id,
            "github_login": login,
            "role": role.as_str(),
            "expires_at_ms": expires_at_ms,
        }
    }))
}

pub async fn revoke_invite(
    db: &impl Db,
    caller: &Principal,
    invite_id: &str,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    let rows = db
        .query(
            "SELECT github_login FROM org_invites WHERE id = ? AND org_id = ? \
             AND accepted_at_ms IS NULL AND revoked_at_ms IS NULL",
            &[invite_id.into(), caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let Some(row) = rows.first() else {
        return Err(ApiError::not_found("no such open invite"));
    };
    let login = text(row, "github_login").map_err(db_failure)?;
    audit::record(
        db,
        &Event::by(
            caller,
            "member.invite.revoke",
            "invite",
            invite_id,
            request_id,
            json!({ "github_login": login }),
        ),
    )
    .await?;
    db.execute(
        "UPDATE org_invites SET revoked_at_ms = ? WHERE id = ? AND org_id = ? \
         AND accepted_at_ms IS NULL AND revoked_at_ms IS NULL",
        &[
            now_ms.into(),
            invite_id.into(),
            caller.org_id.as_str().into(),
        ],
    )
    .await
    .map_err(db_failure)?;
    Ok(json!({ "revoked": true, "invite_id": invite_id }))
}

/// The target's current role in the caller's org; `404` when they are not a member of it.
async fn member_role(db: &impl Db, org_id: &str, user_id: &str) -> Result<Role, ApiError> {
    let rows = db
        .query(
            "SELECT role FROM members WHERE org_id = ? AND user_id = ?",
            &[org_id.into(), user_id.into()],
        )
        .await
        .map_err(db_failure)?;
    let row = rows
        .first()
        .ok_or_else(|| ApiError::not_found("no such member"))?;
    text(row, "role")
        .map_err(db_failure)?
        .parse::<Role>()
        .map_err(|error| ApiError::internal(error.to_string()))
}

pub async fn set_role(
    db: &impl Db,
    caller: &Principal,
    action: &str,
    user_id: &str,
    body: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    let new_role = parse_role(body.get("role"), None)?;
    let current = member_role(db, &caller.org_id, user_id).await?;
    if user_id == caller.user_id {
        return Err(forbidden("you cannot change your own role".to_owned()));
    }
    require_not_above(caller, current, "change")?;
    require_not_above(caller, new_role, "grant")?;
    audit::record(
        db,
        &Event::by(
            caller,
            action,
            "user",
            user_id,
            request_id,
            json!({ "from": current.as_str(), "to": new_role.as_str() }),
        ),
    )
    .await?;
    db.execute(
        "UPDATE members SET role = ?, updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE org_id = ? AND user_id = ?",
        &[
            new_role.as_str().into(),
            caller.org_id.as_str().into(),
            user_id.into(),
        ],
    )
    .await
    .map_err(db_failure)?;
    Ok(json!({ "user_id": user_id, "role": new_role.as_str() }))
}

pub async fn remove_member(
    db: &impl Db,
    caller: &Principal,
    action: &str,
    user_id: &str,
    request_id: &str,
) -> Result<Value, ApiError> {
    let current = member_role(db, &caller.org_id, user_id).await?;
    if user_id == caller.user_id {
        return Err(forbidden("you cannot remove yourself".to_owned()));
    }
    require_not_above(caller, current, "remove")?;
    audit::record(
        db,
        &Event::by(
            caller,
            action,
            "user",
            user_id,
            request_id,
            json!({ "role": current.as_str() }),
        ),
    )
    .await?;
    db.execute(
        "DELETE FROM members WHERE org_id = ? AND user_id = ?",
        &[caller.org_id.as_str().into(), user_id.into()],
    )
    .await
    .map_err(db_failure)?;
    // The session lookup already requires a membership; this also marks the rows revoked.
    db.execute(
        "UPDATE sessions SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE org_id = ? AND user_id = ? AND revoked_at IS NULL",
        &[caller.org_id.as_str().into(), user_id.into()],
    )
    .await
    .map_err(db_failure)?;
    Ok(json!({ "removed": true, "user_id": user_id }))
}
