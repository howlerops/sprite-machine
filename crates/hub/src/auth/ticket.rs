//! Subscribe tickets (SPEC-006, client-api.md §5.2, M2-008h): how a browser, which cannot set an
//! `Authorization` header on a WebSocket, opens `GET /v1/threads/{id}/subscribe`.
//!
//! - [`mint`] (after `authorize_thread` has checked the role and the org): a ticket is `sst_` plus
//!   64 hex characters (256 random bits), bound to the session, user, org and thread, valid for
//!   [`TICKET_TTL_MS`], single use. Only its SHA-256 is stored.
//! - [`consume`] (the subscribe upgrade, no `Authorization` header): claims the ticket with one
//!   conditional `UPDATE`, so exactly one caller wins even when two upgrades race, then rebuilds
//!   the principal from the live session, so a revoked session, device or demoted member loses
//!   the ticket too. Every failure is the same `401`.
//!
//! The ticket is never logged, audited or put in an error. The clock is passed in.

use serde_json::{Value, json};

use super::token::{TOKEN_PREFIX, hash_token, new_id, new_token};
use super::{Principal, store};
use crate::db::{Db, DbError, Param, Role, opt_text, queries, text};
use crate::threads::ApiError;

/// A ticket is valid for 60 seconds.
pub const TICKET_TTL_MS: i64 = 60 * 1000;
/// Every ticket starts with this.
pub const TICKET_PREFIX: &str = "sst_";

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("subscribe ticket: {error}"))
}

/// The one answer for a missing, malformed, reused, expired, other-thread or revoked ticket.
fn refused() -> ApiError {
    ApiError::unauthorized("missing or invalid access token")
}

/// Whether `value` has the shape of a ticket (checked before any database work).
pub fn looks_like_ticket(value: &str) -> bool {
    value
        .strip_prefix(TICKET_PREFIX)
        .is_some_and(|rest| rest.len() == 64 && rest.bytes().all(|b| b.is_ascii_hexdigit()))
}

/// The ticket of a query string (`after=1&ticket=sst_...`), if there is one.
pub fn ticket_in_query(query: Option<&str>) -> Option<&str> {
    query?
        .split('&')
        .find_map(|pair| pair.strip_prefix("ticket="))
}

/// `query` without its `ticket=` pairs, so the ticket never travels past the Worker.
pub fn strip_ticket(query: &str) -> String {
    query
        .split('&')
        .filter(|pair| !pair.is_empty() && !pair.starts_with("ticket="))
        .collect::<Vec<_>>()
        .join("&")
}

/// `POST /v1/threads/{id}/subscribe-ticket`: stores a fresh ticket for `principal` and `thread_id`
/// and returns `{ticket, expires_at}` (the only time the ticket is shown). The caller has passed
/// `authorize_thread` for the route.
pub async fn mint(
    db: &impl Db,
    principal: &Principal,
    thread_id: &str,
    now_ms: i64,
) -> Result<Value, ApiError> {
    let ticket = new_token()?.replacen(TOKEN_PREFIX, TICKET_PREFIX, 1);
    let expires_at_ms = now_ms + TICKET_TTL_MS;
    // Spent and expired tickets are garbage; sweep them with every mint.
    db.execute(
        "DELETE FROM subscribe_tickets WHERE expires_at_ms < ?",
        &[(now_ms - TICKET_TTL_MS).into()],
    )
    .await
    .map_err(db_failure)?;
    db.execute(
        "INSERT INTO subscribe_tickets \
         (id, ticket_hash, session_id, user_id, org_id, thread_id, created_at_ms, expires_at_ms) \
         VALUES (?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            new_id("stk")?.into(),
            hash_token(&ticket).into(),
            Param::opt(principal.session_id.as_deref()),
            principal.user_id.as_str().into(),
            principal.org_id.as_str().into(),
            thread_id.into(),
            now_ms.into(),
            expires_at_ms.into(),
        ],
    )
    .await
    .map_err(db_failure)?;
    let expires_at = u64::try_from(expires_at_ms)
        .map(protocol::turn_event::format_timestamp)
        .map_err(|_| ApiError::internal("clock is before 1970"))?;
    Ok(json!({ "ticket": ticket, "expires_at": expires_at }))
}

/// Trades a ticket for the principal it was minted for, consuming it. `401` (always the same
/// answer) unless the ticket is well formed, unused, unexpired, minted for `thread_id`, its
/// session is still live, the member still has the viewer role, and the thread still belongs to
/// the member's org.
pub async fn consume(
    db: &impl Db,
    dev_fixtures: bool,
    ticket: &str,
    thread_id: &str,
    now_ms: i64,
) -> Result<Principal, ApiError> {
    if !looks_like_ticket(ticket) {
        return Err(refused());
    }
    let ticket_hash = hash_token(ticket);
    // The single-use claim: one conditional UPDATE, so exactly one of two racing upgrades
    // changes the row.
    let claimed = db
        .execute(
            "UPDATE subscribe_tickets SET used_at_ms = ? \
             WHERE ticket_hash = ? AND thread_id = ? AND used_at_ms IS NULL AND expires_at_ms > ?",
            &[
                now_ms.into(),
                ticket_hash.as_str().into(),
                thread_id.into(),
                now_ms.into(),
            ],
        )
        .await
        .map_err(db_failure)?;
    if claimed != 1 {
        return Err(refused());
    }
    let rows = db
        .query(
            "SELECT session_id, user_id, org_id FROM subscribe_tickets WHERE ticket_hash = ?",
            &[ticket_hash.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let row = rows.first().ok_or_else(refused)?;
    let user_id = text(row, "user_id").map_err(db_failure)?;
    let org_id = text(row, "org_id").map_err(db_failure)?;
    let principal = match opt_text(row, "session_id").map_err(db_failure)? {
        Some(session_id) => store::find_by_session_id(db, &session_id)
            .await
            .map_err(db_failure)?
            .ok_or_else(refused)?,
        // A dev ticket (no session) is honoured only while the dev fixtures are on.
        None if dev_fixtures && user_id == super::DEV_USER_ID => {
            store::ensure_dev_principal(db).await.map_err(db_failure)?;
            super::dev_principal()
        }
        None => return Err(refused()),
    };
    if principal.user_id != user_id
        || principal.org_id != org_id
        || !principal.role.allows(Role::Viewer)
    {
        return Err(refused());
    }
    let owner = queries::thread_org(db, thread_id)
        .await
        .map_err(db_failure)?;
    if owner.as_deref() != Some(principal.org_id.as_str()) {
        return Err(refused());
    }
    Ok(principal)
}
