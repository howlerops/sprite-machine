//! The auth endpoints' logic over the `Db` trait: bootstrap, issue and revoke sessions, and the
//! per-request gate for the thread routes. Platform-neutral; `routes.rs` only does HTTP.
//!
//! D1 offers no multi-statement transaction through [`Db`], so the one-time bootstrap is
//! serialised by a single-row claim (primary key, so a concurrent second attempt fails) and a
//! failure after the claim undoes what was written (delete cascade) before reporting.

use serde_json::{Value, json};

use super::policy::{Policy, thread_policy};
use super::token::{self, new_id, new_token, secrets_match};
use super::{Principal, authenticate, bearer_token, store};
use crate::audit::{self, Event};
use crate::db::{Db, DbError, Role, opt_text, queries, text};
use crate::threads::ApiError;

/// Default and maximum session lifetime for [`issue_session`], in seconds.
pub const DEFAULT_TTL_SECONDS: i64 = 24 * 60 * 60;
pub const MAX_TTL_SECONDS: i64 = 90 * 24 * 60 * 60;
/// Lifetime of the session the bootstrap returns.
const BOOTSTRAP_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Lifetime of a browser sign-in session (link redeem or browser device code).
pub const BROWSER_SESSION_TTL_SECONDS: i64 = 7 * 24 * 60 * 60;
const MAX_NAME_CHARS: usize = 128;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("auth: {error}"))
}

fn conflict(message: &str) -> ApiError {
    ApiError::new(409, "conflict", message)
}

/// Parses a JSON object body; an empty body is `{}`.
pub fn parse_body(bytes: &[u8]) -> Result<Value, ApiError> {
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok(json!({}));
    }
    match serde_json::from_slice::<Value>(bytes) {
        Ok(value) if value.is_object() => Ok(value),
        _ => Err(ApiError::invalid_request("body must be a JSON object")),
    }
}

fn required_str(body: &Value, key: &str) -> Result<String, ApiError> {
    optional_str(body, key)?.ok_or_else(|| ApiError::invalid_request(format!("{key} is required")))
}

fn optional_str(body: &Value, key: &str) -> Result<Option<String>, ApiError> {
    match body.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => {
            let text = text.trim();
            if text.is_empty() || text.chars().count() > MAX_NAME_CHARS {
                Err(ApiError::invalid_request(format!(
                    "{key} must be 1-{MAX_NAME_CHARS} characters"
                )))
            } else {
                Ok(Some(text.to_owned()))
            }
        }
        Some(_) => Err(ApiError::invalid_request(format!("{key} must be a string"))),
    }
}

fn slugify(name: &str) -> String {
    let mut slug = String::new();
    for c in name.chars().flat_map(char::to_lowercase) {
        if c.is_ascii_alphanumeric() {
            slug.push(c);
        } else if !slug.ends_with('-') && !slug.is_empty() {
            slug.push('-');
        }
    }
    let slug = slug.trim_end_matches('-');
    let slug: String = slug.chars().take(40).collect();
    if slug.is_empty() {
        "org".to_owned()
    } else {
        slug
    }
}

/// What a session issue returns (the token only here, once).
struct Issued {
    user_id: String,
    device_id: String,
    session_id: String,
    token: String,
    expires_at: String,
}

impl Issued {
    fn to_json(&self, org_id: &str, role: Role, display_name: &str, email: Option<&str>) -> Value {
        json!({
            "session": {
                "id": self.session_id,
                "token": self.token,
                "expires_at": self.expires_at,
            },
            "user": { "id": self.user_id, "display_name": display_name, "email": email },
            "device_id": self.device_id,
            "org_id": org_id,
            "role": role.as_str(),
        })
    }
}

/// Creates a user, their membership, a device and a session.
/// On a failure the new user is deleted again (cascade).
async fn create_identity(
    db: &impl Db,
    org_id: &str,
    role: Role,
    display_name: &str,
    email: Option<&str>,
    device_name: &str,
    ttl_seconds: i64,
) -> Result<Issued, ApiError> {
    let user_id = new_id("usr")?;
    let device_id = new_id("dev")?;
    let session_id = new_id("ses")?;
    let token = new_token()?;
    let result = async {
        store::insert_user(db, &user_id, display_name, email).await?;
        store::insert_member(db, org_id, &user_id, role).await?;
        store::insert_device(db, &device_id, org_id, &user_id, device_name).await?;
        store::insert_session(
            db,
            &session_id,
            &token::hash_token(&token),
            &user_id,
            &device_id,
            org_id,
            ttl_seconds,
        )
        .await
    }
    .await;
    match result {
        Ok(expires_at) => Ok(Issued {
            user_id,
            device_id,
            session_id,
            token,
            expires_at,
        }),
        Err(error) => {
            let _ = store::delete_user(db, &user_id).await;
            Err(db_failure(error))
        }
    }
}

/// `POST /v1/auth/bootstrap`: creates the first org, its owner and a session, exactly once.
/// `configured` is the `BOOTSTRAP_OWNER_TOKEN` secret; `authorization` is the request header.
pub async fn bootstrap(
    db: &impl Db,
    configured: Option<&str>,
    authorization: Option<&str>,
    body: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    let denied = || ApiError::unauthorized("missing or invalid bootstrap token");
    let configured = configured
        .map(str::trim)
        .filter(|secret| !secret.is_empty())
        .ok_or_else(denied)?;
    let presented = bearer_token(authorization).ok_or_else(denied)?;
    if !secrets_match(configured, presented) {
        return Err(denied());
    }

    let org_name = required_str(body, "org_name")?;
    let user_name = required_str(body, "user_name")?;
    let email = optional_str(body, "user_email")?;

    if store::bootstrap_claimed(db).await.map_err(db_failure)? {
        return Err(conflict("this hub is already bootstrapped"));
    }
    if let Err(error) = store::claim_bootstrap(db).await {
        // The primary key refused: someone else claimed it between the check and now.
        return match store::bootstrap_claimed(db).await {
            Ok(true) => Err(conflict("this hub is already bootstrapped")),
            _ => Err(db_failure(error)),
        };
    }

    let org_id = new_id("org")?;
    let slug = format!("{}-{}", slugify(&org_name), &new_id("s")?[2..6]);
    let user_id = new_id("usr")?;
    let device_id = new_id("dev")?;
    let session_id = new_id("ses")?;
    let session_token = new_token()?;

    let written = async {
        store::insert_org(
            db,
            &store::NewOrgRow {
                id: &org_id,
                slug: &slug,
                name: &org_name,
            },
        )
        .await?;
        store::insert_user(db, &user_id, &user_name, email.as_deref()).await?;
        store::insert_member(db, &org_id, &user_id, Role::Owner).await?;
        store::insert_device(db, &device_id, &org_id, &user_id, "bootstrap").await?;
        let expires_at = store::insert_session(
            db,
            &session_id,
            &token::hash_token(&session_token),
            &user_id,
            &device_id,
            &org_id,
            BOOTSTRAP_TTL_SECONDS,
        )
        .await?;
        store::finish_bootstrap(db, &org_id, &user_id).await?;
        Ok::<_, DbError>(expires_at)
    }
    .await;
    let expires_at = match written {
        Ok(expires_at) => expires_at,
        Err(error) => {
            let _ = store::release_bootstrap(db, &org_id, &user_id).await;
            return Err(db_failure(error));
        }
    };

    let owner = Principal {
        user_id: user_id.clone(),
        device_id: device_id.clone(),
        org_id: org_id.clone(),
        role: Role::Owner,
        display_name: user_name.clone(),
        session_id: Some(session_id.clone()),
    };
    let event = Event::by(
        &owner,
        "auth.bootstrap",
        "org",
        &org_id,
        request_id,
        json!({ "org_name": org_name, "slug": slug, "user_id": user_id, "session_id": session_id }),
    );
    if let Err(error) = audit::record(db, &event).await {
        let _ = store::release_bootstrap(db, &org_id, &user_id).await;
        return Err(error);
    }

    let issued = Issued {
        user_id,
        device_id,
        session_id,
        token: session_token,
        expires_at,
    };
    let mut out = issued.to_json(&org_id, Role::Owner, &user_name, email.as_deref());
    out["org"] = json!({ "id": org_id, "slug": slug, "name": org_name });
    Ok(out)
}

/// `POST /v1/auth/sessions`: an owner or admin issues a session for a new user of their org.
/// Body: `role` (default `member`; an admin may not grant `admin` or `owner`), `display_name`,
/// `device_name`, `ttl_seconds` (1 to 90 days, default 1 day).
pub async fn issue_session(
    db: &impl Db,
    caller: &Principal,
    body: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    let role = match optional_str(body, "role")? {
        None => Role::Member,
        Some(text) => text
            .parse::<Role>()
            .map_err(|error| ApiError::invalid_request(error.to_string()))?,
    };
    if caller.role != Role::Owner && role.rank() >= caller.role.rank() {
        return Err(ApiError::new(
            403,
            "forbidden",
            format!("a {} cannot grant the {role} role", caller.role),
        ));
    }
    let display_name = optional_str(body, "display_name")?.unwrap_or_else(|| "member".to_owned());
    let device_name = optional_str(body, "device_name")?.unwrap_or_else(|| "session".to_owned());
    let ttl = match body.get("ttl_seconds") {
        None | Some(Value::Null) => DEFAULT_TTL_SECONDS,
        Some(value) => value
            .as_i64()
            .filter(|ttl| (1..=MAX_TTL_SECONDS).contains(ttl))
            .ok_or_else(|| {
                ApiError::invalid_request(format!(
                    "ttl_seconds must be an integer from 1 to {MAX_TTL_SECONDS}"
                ))
            })?,
    };

    let issued = create_identity(
        db,
        &caller.org_id,
        role,
        &display_name,
        None,
        &device_name,
        ttl,
    )
    .await?;
    let event = Event::by(
        caller,
        "auth.session.issue",
        "session",
        &issued.session_id,
        request_id,
        json!({
            "role": role.as_str(),
            "user_id": issued.user_id,
            "device_id": issued.device_id,
            "expires_at": issued.expires_at,
        }),
    );
    if let Err(error) = audit::record(db, &event).await {
        let _ = store::delete_user(db, &issued.user_id).await;
        return Err(error);
    }
    Ok(issued.to_json(&caller.org_id, role, &display_name, None))
}

/// A session issued to an existing member of an org (browser sign-in, M2-003c).
pub struct MemberSession {
    pub user_id: String,
    pub device_id: String,
    pub session_id: String,
    pub role: Role,
    pub expires_at: String,
    /// The `smt_` token: shown to the caller once, never stored or logged.
    token: String,
    user: Value,
    org: Value,
}

impl MemberSession {
    /// `{token, expires_at, user, org, role}`: the sign-in response bodies (client-api §5.7).
    pub fn to_json(&self) -> Value {
        json!({
            "token": self.token,
            "expires_at": self.expires_at,
            "user": self.user,
            "org": self.org,
            "role": self.role.as_str(),
        })
    }
}

/// Issues a session for `user_id` in `org_id` on a new device named `device_name`, with the
/// user's role capped at `cap`. `None` when the user is not a member of the org.
pub async fn issue_member_session(
    db: &impl Db,
    user_id: &str,
    org_id: &str,
    cap: Role,
    device_name: &str,
    ttl_seconds: i64,
) -> Result<Option<MemberSession>, ApiError> {
    let rows = db
        .query(
            "SELECT m.role AS role, u.display_name AS display_name, u.email AS email, \
             o.name AS org_name FROM members m JOIN users u ON u.id = m.user_id \
             JOIN orgs o ON o.id = m.org_id WHERE m.org_id = ? AND m.user_id = ?",
            &[org_id.into(), user_id.into()],
        )
        .await
        .map_err(db_failure)?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    let member_role = text(row, "role")
        .map_err(db_failure)?
        .parse::<Role>()
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let user = json!({
        "id": user_id,
        "display_name": text(row, "display_name").map_err(db_failure)?,
        "email": opt_text(row, "email").map_err(db_failure)?,
    });
    let org = json!({ "id": org_id, "name": text(row, "org_name").map_err(db_failure)? });

    let device_id = new_id("dev")?;
    let session_id = new_id("ses")?;
    let session_token = new_token()?;
    store::insert_device(db, &device_id, org_id, user_id, device_name)
        .await
        .map_err(db_failure)?;
    let inserted = async {
        let expires_at = store::insert_session(
            db,
            &session_id,
            &token::hash_token(&session_token),
            user_id,
            &device_id,
            org_id,
            ttl_seconds,
        )
        .await?;
        store::set_role_cap(db, &session_id, cap).await?;
        Ok::<_, DbError>(expires_at)
    }
    .await;
    match inserted {
        Ok(expires_at) => Ok(Some(MemberSession {
            user_id: user_id.to_owned(),
            device_id,
            session_id,
            role: member_role.min(cap),
            expires_at,
            token: session_token,
            user,
            org,
        })),
        Err(error) => {
            let _ = store::delete_device(db, &device_id).await;
            Err(db_failure(error))
        }
    }
}

/// Undo of [`issue_member_session`] (its device and, by cascade, its session), best effort.
pub async fn discard_member_session(db: &impl Db, session: &MemberSession) {
    let _ = store::delete_device(db, &session.device_id).await;
}

/// `DELETE /v1/auth/sessions/current`: the caller revokes the session they are using.
pub async fn revoke_current(
    db: &impl Db,
    caller: &Principal,
    request_id: &str,
) -> Result<Value, ApiError> {
    let Some(session_id) = caller.session_id.as_deref() else {
        return Err(ApiError::invalid_request(
            "this credential is not a session, so there is nothing to revoke",
        ));
    };
    revoke(db, caller, session_id, request_id, "current").await
}

/// `DELETE /v1/auth/sessions/{id}`: an admin revokes any session of their org.
pub async fn revoke_other(
    db: &impl Db,
    caller: &Principal,
    session_id: &str,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    if !store::session_in_org(db, session_id, &caller.org_id)
        .await
        .map_err(db_failure)?
    {
        return Err(ApiError::not_found("no such session"));
    }
    revoke(db, caller, session_id, request_id, "by_id").await
}

async fn revoke(
    db: &impl Db,
    caller: &Principal,
    session_id: &str,
    request_id: &str,
    via: &str,
) -> Result<Value, ApiError> {
    let event = Event::by(
        caller,
        "auth.session.revoke",
        "session",
        session_id,
        request_id,
        json!({ "via": via, "own": caller.session_id.as_deref() == Some(session_id) }),
    );
    audit::record(db, &event).await?;
    store::revoke_session(db, session_id)
        .await
        .map_err(db_failure)?;
    Ok(json!({ "revoked": true, "session_id": session_id }))
}

/// The per-request gate of a thread route: authenticate, find the route's policy, check the
/// role (`403 forbidden`), check the thread belongs to the caller's org (`404`), and write the audit row for a mutation before it is forwarded (if
/// the audit write fails the request fails and nothing mutates).
pub async fn authorize_thread(
    db: &impl Db,
    authorization: Option<&str>,
    dev_fixtures: bool,
    method: &str,
    path: &str,
    request_id: &str,
) -> Result<(Principal, Policy), ApiError> {
    let principal = authenticate(db, authorization, dev_fixtures).await?;
    let policy = thread_policy(method, path)
        .ok_or_else(|| ApiError::not_found(format!("no route for {method} {path}")))?;
    principal.require(policy.role)?;
    if !protocol::turn_event::is_valid_id(&policy.thread_id) {
        return Err(ApiError::invalid_request(
            "thread_id must be 1-128 characters of [A-Za-z0-9_:.-]",
        ));
    }
    // A thread is visible to the members of the org that owns its machine: no host row, or
    // another org's, is one `404` (before any audit row or Thread DO call).
    let owner = queries::thread_org(db, &policy.thread_id)
        .await
        .map_err(db_failure)?;
    if owner.as_deref() != Some(principal.org_id.as_str()) {
        return Err(ApiError::not_found(format!(
            "no thread {}",
            policy.thread_id
        )));
    }
    if let Some(action) = policy.action {
        let mut detail = json!({ "thread_id": policy.thread_id });
        if let Some((key, value)) = &policy.extra {
            detail[*key] = Value::String(value.clone());
        }
        let event = Event::by(
            &principal,
            action,
            "thread",
            &policy.thread_id,
            request_id,
            detail,
        );
        audit::record(db, &event).await?;
    }
    Ok((principal, policy))
}

#[cfg(test)]
mod tests {
    use super::slugify;

    #[test]
    fn slugs_are_lowercase_dashed_and_never_empty() {
        assert_eq!(slugify("Acme  Corp!"), "acme-corp");
        assert_eq!(slugify("***"), "org");
    }
}
