//! Hub auth (M2-003a, plan §4.2 "Auth" and §4.8): session tokens, the auth extractor, roles.
//!
//! - [`authenticate`] turns an `Authorization: Bearer <token>` header into a [`Principal`]: a
//!   session token (`smt_...`, hash looked up in D1, expiry and revocation honoured) or, only
//!   with `DEV_FIXTURES=1`, the literal `dev` (a dev user that owns a dev org).
//! - [`Principal::require`] is the role gate; [`policy`] says which role each thread route needs
//!   and which action it audits; [`service::authorize_thread`] does all three for one request.
//! - [`service`] holds the auth endpoints' logic (bootstrap, issue and revoke sessions) over the
//!   `Db` trait, so it runs natively in the unit tests. `routes` (wasm32 only) is the axum glue.
//!
//! The actor is derived only from the session, never from a request body. Tokens and the
//! bootstrap secret are never stored in clear, logged or put in an audit row.
//!
//! # What a route does
//!
//! ```text
//! let principal = service::authorize_thread(&db, authorization, dev_fixtures, method, path, request_id).await?;
//! // principal.actor() is the Actor to forward; the role gate and the audit row are done.
//! ```
//! Routes outside the thread family call [`authenticate`], then [`Principal::require`], then
//! `audit::record` for every mutation.

use protocol::tunnel::{Actor, ActorKind};

use crate::db::{Db, Role};
use crate::threads::ApiError;

pub mod policy;
#[cfg(target_arch = "wasm32")]
pub mod routes;
pub mod service;
mod store;
pub mod token;

#[cfg(test)]
mod tests;

/// The dev org, user and device behind `Bearer dev` (created on first use).
pub const DEV_ORG_ID: &str = "org_dev";
pub const DEV_USER_ID: &str = "usr_dev";
pub const DEV_DEVICE_ID: &str = "dev_dev";

/// Who is calling: the user, device and org of a valid session, and the user's current role in
/// that org (looked up on every request, so a demotion applies at once).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Principal {
    pub user_id: String,
    pub device_id: String,
    pub org_id: String,
    pub role: Role,
    pub display_name: String,
    /// The session behind this request; `None` for `Bearer dev`.
    pub session_id: Option<String>,
}

impl Principal {
    /// The actor recorded on events: `{kind:"user", id, name?, device_id}`.
    pub fn actor(&self) -> Actor {
        Actor {
            kind: ActorKind::User,
            id: self.user_id.clone(),
            name: (!self.display_name.is_empty()).then(|| self.display_name.clone()),
            device_id: Some(self.device_id.clone()),
        }
    }

    /// `403 forbidden` unless the caller's role is at least `required`.
    pub fn require(&self, required: Role) -> Result<(), ApiError> {
        if self.role.allows(required) {
            Ok(())
        } else {
            Err(ApiError::new(
                403,
                "forbidden",
                format!("this needs the {required} role; you are {}", self.role),
            ))
        }
    }
}

/// `true` when the `DEV_FIXTURES` var value turns on the dev fixtures.
pub fn dev_fixtures_enabled(var: Option<&str>) -> bool {
    var.is_some_and(|value| value.trim() == "1")
}

/// The token of an `Authorization: Bearer <token>` header value.
pub fn bearer_token(authorization: Option<&str>) -> Option<&str> {
    authorization
        .and_then(|value| value.strip_prefix("Bearer "))
        .map(str::trim)
        .filter(|token| !token.is_empty())
}

fn unauthorized() -> ApiError {
    ApiError::unauthorized("missing or invalid access token")
}

fn db_failure(error: crate::db::DbError) -> ApiError {
    ApiError::internal(format!("auth: {error}"))
}

/// Derives the caller from an `Authorization` header value. A missing, unknown, expired or
/// revoked token is `401 unauthorized`; `dev` works only with `dev_fixtures`.
pub async fn authenticate(
    db: &impl Db,
    authorization: Option<&str>,
    dev_fixtures: bool,
) -> Result<Principal, ApiError> {
    let token = bearer_token(authorization).ok_or_else(unauthorized)?;
    if token == "dev" {
        if !dev_fixtures {
            return Err(unauthorized());
        }
        store::ensure_dev_principal(db).await.map_err(db_failure)?;
        return Ok(Principal {
            user_id: DEV_USER_ID.to_owned(),
            device_id: DEV_DEVICE_ID.to_owned(),
            org_id: DEV_ORG_ID.to_owned(),
            role: Role::Owner,
            display_name: String::new(),
            session_id: None,
        });
    }
    if !token.starts_with(token::TOKEN_PREFIX) {
        return Err(unauthorized());
    }
    let hash = token::hash_token(token);
    let found = store::find_by_hash(db, &hash).await.map_err(db_failure)?;
    match found {
        Some((stored_hash, principal)) if token::ct_eq(stored_hash.as_bytes(), hash.as_bytes()) => {
            Ok(principal)
        }
        _ => Err(unauthorized()),
    }
}
