//! Browser sign-in, flow A (client-api.md §5.7, M2-003c): a paired machine mints a one-time link,
//! the phone page redeems it for a session.
//!
//! - [`mint`] (`POST /v1/devices/signin-links`, `Bearer smm_...`): only a device-origin machine
//!   (one a member approved) may mint; the link is bound to that member and the machine's org.
//! - [`redeem`] (`POST /v1/auth/links/redeem`, no auth): trades the code for a session of that
//!   member, role capped at `member`, device `web`.
//!
//! The code is 256 random bits and single use; only its SHA-256 is stored. It is never put in an
//! audit row, an error or a log. A used, expired, revoked-machine or unknown code is one answer:
//! `410 expired`. The clock is passed in.

use serde_json::{Value, json};

use super::codes::{hash_link_code, looks_like_link_code, new_link_code};
use super::limits::Limited;
use crate::audit::{self, Event};
use crate::auth::service::{
    BROWSER_SESSION_TTL_SECONDS, discard_member_session, issue_member_session,
};
use crate::auth::token::new_id;
use crate::db::{Db, DbError, Role, int, opt_text, text};
use crate::machines::credential::{CREDENTIAL_PREFIX, hash};
use crate::threads::ApiError;

/// A link is valid for 5 minutes.
pub const LINK_TTL_MS: i64 = 5 * 60 * 1000;
/// Unused, unexpired links one machine may hold at once (`SIGUSR1` mints a fresh one each time).
pub const MAX_LIVE_PER_MACHINE: i64 = 20;
/// The device name of a browser session.
pub const WEB_DEVICE_NAME: &str = "web";

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("signin: {error}"))
}

fn expired() -> ApiError {
    ApiError::new(
        410,
        "expired",
        "this sign-in link is used, expired or unknown",
    )
}

/// `POST /v1/devices/signin-links`: authenticated by the machine credential. `base_url` is the
/// hub's public origin (scheme and host of the request).
pub async fn mint(
    db: &impl Db,
    authorization: Option<&str>,
    base_url: &str,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    let denied = || ApiError::unauthorized("invalid machine credential");
    let credential = crate::auth::bearer_token(authorization)
        .filter(|credential| credential.starts_with(CREDENTIAL_PREFIX))
        .ok_or_else(denied)?;
    let rows = db
        .query(
            "SELECT m.id AS id, m.org_id AS org_id, m.origin AS origin, \
             m.revoked_at IS NOT NULL AS revoked, m.approved_by_user_id AS user_id, \
             p.approved_by_device_id AS device_id \
             FROM machines m LEFT JOIN device_pairings p ON p.machine_id = m.id \
             WHERE m.credential_hash = ?",
            &[hash(credential).into()],
        )
        .await
        .map_err(db_failure)?;
    let Some(row) = rows.first() else {
        return Err(denied());
    };
    if int(row, "revoked").map_err(db_failure)? == 1 {
        return Err(denied());
    }
    let machine_id = text(row, "id").map_err(db_failure)?;
    let org_id = text(row, "org_id").map_err(db_failure)?;
    let forbidden = || {
        ApiError::new(
            403,
            "forbidden",
            "only a machine a member approved can mint sign-in links",
        )
    };
    let (true, Some(user_id), Some(device_id)) = (
        text(row, "origin").map_err(db_failure)? == "device",
        opt_text(row, "user_id").map_err(db_failure)?,
        opt_text(row, "device_id").map_err(db_failure)?,
    ) else {
        return Err(forbidden());
    };
    let member = db
        .query(
            "SELECT role FROM members WHERE org_id = ? AND user_id = ?",
            &[org_id.as_str().into(), user_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    if member.is_empty() {
        return Err(forbidden());
    }

    let live = db
        .query(
            "SELECT COUNT(*) AS n, MIN(expires_at_ms) AS first_free FROM signin_links \
             WHERE machine_id = ? AND used_at_ms IS NULL AND expires_at_ms > ?",
            &[machine_id.as_str().into(), now_ms.into()],
        )
        .await
        .map_err(db_failure)?;
    if let Some(row) = live.first()
        && int(row, "n").map_err(db_failure)? >= MAX_LIVE_PER_MACHINE
    {
        let free_at = row
            .get("first_free")
            .and_then(Value::as_i64)
            .unwrap_or(now_ms + LINK_TTL_MS);
        return Err(Limited {
            retry_after_seconds: ((free_at - now_ms).max(0) + 999) / 1000,
        }
        .into_api());
    }

    let code = new_link_code()?;
    let id = new_id("sln")?;
    let expires_at_ms = now_ms + LINK_TTL_MS;
    db.execute(
        "INSERT INTO signin_links (id, code_hash, org_id, user_id, machine_id, created_at_ms, \
         expires_at_ms) VALUES (?, ?, ?, ?, ?, ?, ?)",
        &[
            id.as_str().into(),
            hash_link_code(&code).into(),
            org_id.as_str().into(),
            user_id.as_str().into(),
            machine_id.as_str().into(),
            now_ms.into(),
            expires_at_ms.into(),
        ],
    )
    .await
    .map_err(db_failure)?;
    let audited = audit::record(
        db,
        &Event {
            org_id: &org_id,
            actor_user_id: &user_id,
            actor_device_id: &device_id,
            action: "device.signin_link",
            target_kind: "machine",
            target_id: &machine_id,
            request_id,
            detail: json!({ "link_id": id }),
        },
    )
    .await;
    if let Err(error) = audited {
        let _ = db
            .execute(
                "DELETE FROM signin_links WHERE id = ?",
                &[id.as_str().into()],
            )
            .await;
        return Err(error);
    }
    Ok(json!({
        "url": format!("{}/pair?code={code}", base_url.trim_end_matches('/')),
        "expires_at": iso(expires_at_ms),
    }))
}

/// Epoch milliseconds as `YYYY-MM-DDTHH:MM:SS.mmmZ` (UTC), the format of every other timestamp.
pub fn iso(ms: i64) -> String {
    let days = ms.div_euclid(86_400_000);
    let rest = ms.rem_euclid(86_400_000);
    // Civil-from-days (Howard Hinnant).
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = doy - (153 * mp + 2) / 5 + 1;
    let month = if mp < 10 { mp + 3 } else { mp - 9 };
    let year = yoe + era * 400 + i64::from(month <= 2);
    format!(
        "{year:04}-{month:02}-{day:02}T{:02}:{:02}:{:02}.{:03}Z",
        rest / 3_600_000,
        rest / 60_000 % 60,
        rest / 1000 % 60,
        rest % 1000
    )
}

/// `POST /v1/auth/links/redeem {code}`: no auth, the code is the secret. `201` body is
/// `{token, expires_at, user, org, role}`.
pub async fn redeem(
    db: &impl Db,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    let code = body
        .get("code")
        .and_then(Value::as_str)
        .ok_or_else(|| ApiError::invalid_request("code is required"))?;
    if !looks_like_link_code(code) {
        return Err(expired());
    }
    let code_hash = hash_link_code(code);
    // The single-use claim: exactly one caller changes the row.
    let claimed = db
        .execute(
            "UPDATE signin_links SET used_at_ms = ? \
             WHERE code_hash = ? AND used_at_ms IS NULL AND expires_at_ms > ?",
            &[now_ms.into(), code_hash.as_str().into(), now_ms.into()],
        )
        .await
        .map_err(db_failure)?;
    if claimed == 0 {
        return Err(expired());
    }
    let rows = db
        .query(
            "SELECT l.id AS id, l.org_id AS org_id, l.user_id AS user_id, \
             l.machine_id AS machine_id, m.revoked_at IS NOT NULL AS revoked \
             FROM signin_links l LEFT JOIN machines m ON m.id = l.machine_id \
             WHERE l.code_hash = ?",
            &[code_hash.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let row = rows.first().ok_or_else(expired)?;
    // A machine revoked since it minted the link takes the link with it.
    if int(row, "revoked").unwrap_or(1) == 1 {
        return Err(expired());
    }
    let link_id = text(row, "id").map_err(db_failure)?;
    let org_id = text(row, "org_id").map_err(db_failure)?;
    let user_id = text(row, "user_id").map_err(db_failure)?;
    let machine_id = text(row, "machine_id").map_err(db_failure)?;

    let session = issue_member_session(
        db,
        &user_id,
        &org_id,
        Role::Member,
        WEB_DEVICE_NAME,
        BROWSER_SESSION_TTL_SECONDS,
    )
    .await?
    .ok_or_else(expired)?;
    let audited = audit::record(
        db,
        &Event {
            org_id: &org_id,
            actor_user_id: &user_id,
            actor_device_id: &session.device_id,
            action: "session.link_redeem",
            target_kind: "session",
            target_id: &session.session_id,
            request_id,
            detail: json!({
                "link_id": link_id,
                "machine_id": machine_id,
                "role": session.role.as_str(),
            }),
        },
    )
    .await;
    if let Err(error) = audited {
        discard_member_session(db, &session).await;
        return Err(error);
    }
    Ok(session.to_json())
}
