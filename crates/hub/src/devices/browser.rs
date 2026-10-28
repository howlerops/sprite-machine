//! Browser sign-in, flow B (client-api.md §5.7, M2-003c): the RFC 8628 device-code flow of `sm
//! login`, for a user session instead of a machine.
//!
//! [`start`] (`POST /v1/auth/device/code`) -> a signed-in member runs [`approve`]
//! (`POST /v1/auth/device/approve {user_code}`) -> the page polls [`poll`]
//! (`POST /v1/auth/device/token {device_code}`) and receives the session token once.
//!
//! Same codes, errors, polling rules and rate limits as device pairing (`limits`: per-IP start
//! window, cap on live pairings, failed user-code attempts per user and org), but its own table
//! (`browser_pairings`), so a machine pairing's device code can never be traded for a session.
//! The session is the approver's own, in the approver's org, role capped at `member`, device
//! `web`. The clock is passed in.

use serde_json::{Value, json};

use super::codes::{display_user_code, hash_device_code, new_device_code, new_user_code};
use super::limits;
use super::service::{
    EXPIRES_IN_SECONDS, INTERVAL_SECONDS, PollError, SLOW_DOWN_STEP_SECONDS, user_code_of,
};
use super::signin::WEB_DEVICE_NAME;
use crate::audit::{self, Event};
use crate::auth::Principal;
use crate::auth::service::{
    BROWSER_SESSION_TTL_SECONDS, discard_member_session, issue_member_session,
};
use crate::auth::token::new_id;
use crate::db::{Db, DbError, Role, int, opt_text, text};
use crate::threads::ApiError;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("browser pairing: {error}"))
}

/// `POST /v1/auth/device/code`: unauthenticated, rate limited like `POST /v1/devices/code`.
pub async fn start(
    db: &impl Db,
    base_url: &str,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Value, ApiError> {
    let _ = limits::purge(db, now_ms).await;
    if let Some(limited) = limits::check_start_in(db, "browser_pairings", client_ip, now_ms).await?
    {
        return Err(limited.into_api());
    }
    let device_code = new_device_code()?;
    let id = new_id("bpr")?;
    let mut user_code = String::new();
    let mut inserted = false;
    for _ in 0..5 {
        user_code = new_user_code()?;
        let result = db
            .execute(
                "INSERT INTO browser_pairings (id, device_code_hash, user_code, created_at_ms, \
                 expires_at_ms, interval_seconds) VALUES (?, ?, ?, ?, ?, ?)",
                &[
                    id.as_str().into(),
                    hash_device_code(&device_code).into(),
                    user_code.as_str().into(),
                    now_ms.into(),
                    (now_ms + EXPIRES_IN_SECONDS * 1000).into(),
                    INTERVAL_SECONDS.into(),
                ],
            )
            .await;
        match result {
            Ok(_) => {
                inserted = true;
                break;
            }
            Err(DbError::Backend(message)) if message.contains("user_code") => {}
            Err(error) => return Err(db_failure(error)),
        }
    }
    if !inserted {
        return Err(ApiError::internal("could not allocate a user code"));
    }
    let base = base_url.trim_end_matches('/');
    let shown = display_user_code(&user_code);
    Ok(json!({
        "device_code": device_code,
        "user_code": shown,
        "verification_uri": format!("{base}/pair/device"),
        "verification_uri_complete": format!("{base}/pair/device?user_code={shown}"),
        "expires_in": EXPIRES_IN_SECONDS,
        "interval": INTERVAL_SECONDS,
    }))
}

/// `POST /v1/auth/device/approve {user_code}`: a signed-in member. The page's next poll gets a
/// session for the caller. Failed user-code attempts count against the caller's user and org
/// (`429` once over the limit), the same counters as `POST /v1/devices/approve`.
pub async fn approve(
    db: &impl Db,
    caller: &Principal,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Member)?;
    let user_code = user_code_of(body)?;
    if let Some(limited) =
        limits::check_approve(db, &caller.user_id, &caller.org_id, now_ms).await?
    {
        return Err(limited.into_api());
    }
    let changed = db
        .execute(
            "UPDATE browser_pairings SET status = 'approved', org_id = ?, approved_by_user_id = ? \
             WHERE user_code = ? AND status = 'pending' AND expires_at_ms > ?",
            &[
                caller.org_id.as_str().into(),
                caller.user_id.as_str().into(),
                user_code.as_str().into(),
                now_ms.into(),
            ],
        )
        .await
        .map_err(db_failure)?;
    if changed == 0 {
        limits::record_approve_failure(db, &caller.user_id, &caller.org_id, now_ms).await?;
        return Err(ApiError::not_found(
            "no pending sign-in with that code (unknown, expired or already answered)",
        ));
    }
    let rows = db
        .query(
            "SELECT id FROM browser_pairings WHERE user_code = ?",
            &[user_code.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let id = rows
        .first()
        .map(|row| text(row, "id"))
        .transpose()
        .map_err(db_failure)?
        .ok_or_else(|| ApiError::internal("pairing row vanished"))?;
    audit::record(
        db,
        &Event::by(
            caller,
            "auth.device.approve",
            "browser_pairing",
            &id,
            request_id,
            json!({}),
        ),
    )
    .await?;
    Ok(json!({ "status": "approved" }))
}

/// `POST /v1/auth/device/token {device_code}`: unauthenticated (the device code is the secret).
/// `Ok` is the session (`{token, expires_at, user, org, role}`), exactly once.
pub async fn poll(
    db: &impl Db,
    device_code: &str,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, PollError> {
    let rows = db
        .query(
            "SELECT id, status, expires_at_ms, interval_seconds, last_poll_at_ms, org_id, \
             approved_by_user_id FROM browser_pairings WHERE device_code_hash = ?",
            &[hash_device_code(device_code).into()],
        )
        .await
        .map_err(db_failure)?;
    let Some(row) = rows.first() else {
        return Err(rfc("invalid_grant"));
    };
    let id = text(row, "id").map_err(db_failure)?;
    let status = text(row, "status").map_err(db_failure)?;
    let expires_at_ms = int(row, "expires_at_ms").map_err(db_failure)?;
    let interval_seconds = int(row, "interval_seconds").map_err(db_failure)?;
    let last_poll = row.get("last_poll_at_ms").and_then(Value::as_i64);
    let org_id = opt_text(row, "org_id").map_err(db_failure)?;
    let user_id = opt_text(row, "approved_by_user_id").map_err(db_failure)?;

    if status == "consumed" {
        return Err(rfc("invalid_grant"));
    }
    if expires_at_ms <= now_ms {
        return Err(rfc("expired_token"));
    }
    let too_fast = last_poll.is_some_and(|last| now_ms - last < interval_seconds * 1000);
    let interval = if too_fast {
        interval_seconds + SLOW_DOWN_STEP_SECONDS
    } else {
        interval_seconds
    };
    db.execute(
        "UPDATE browser_pairings SET last_poll_at_ms = ?, interval_seconds = ? WHERE id = ?",
        &[now_ms.into(), interval.into(), id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    if too_fast {
        return Err(PollError::Rfc {
            code: "slow_down",
            interval: Some(interval),
        });
    }
    if status == "pending" {
        return Err(rfc("authorization_pending"));
    }
    let (Some(org_id), Some(user_id)) = (org_id, user_id) else {
        return Err(ApiError::internal("approved pairing is incomplete").into());
    };

    // Claim it (`approved` -> `consumed`) before minting, so two racing polls cannot both get
    // a token.
    let claimed = db
        .execute(
            "UPDATE browser_pairings SET status = 'consumed' WHERE id = ? AND status = 'approved'",
            &[id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    if claimed == 0 {
        return Err(rfc("invalid_grant"));
    }
    let session = issue_member_session(
        db,
        &user_id,
        &org_id,
        Role::Member,
        WEB_DEVICE_NAME,
        BROWSER_SESSION_TTL_SECONDS,
    )
    .await?
    // The approver left the org between approving and this poll.
    .ok_or_else(|| rfc("access_denied"))?;
    let audited = audit::record(
        db,
        &Event {
            org_id: &org_id,
            actor_user_id: &user_id,
            actor_device_id: &session.device_id,
            action: "auth.device.grant",
            target_kind: "session",
            target_id: &session.session_id,
            request_id,
            detail: json!({ "pairing_id": id, "role": session.role.as_str() }),
        },
    )
    .await;
    if let Err(error) = audited {
        discard_member_session(db, &session).await;
        return Err(error.into());
    }
    Ok(session.to_json())
}

fn rfc(code: &'static str) -> PollError {
    PollError::Rfc {
        code,
        interval: None,
    }
}
