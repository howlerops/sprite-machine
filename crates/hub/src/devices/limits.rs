//! Rate limits for device pairing (RFC 8628 §5.1 user-code brute force, M2B-005a).
//!
//! Sliding windows over `device_attempts` (one row per counted attempt, migration 0007):
//!
//! - failed user-code attempts on approve/deny: per user and per org, 5 per minute and 20 per
//!   hour. A success writes nothing, so it never resets (or consumes) anyone's counter;
//! - `POST /v1/devices/code`: per `CF-Connecting-IP` (when present) 10 per minute, and at most
//!   [`MAX_PENDING`] live pairings on the hub;
//! - `POST /v1/devices/token` (the runner's poll, M2B-005c): per `CF-Connecting-IP` (when
//!   present) 60 per minute. Every call counts, since each one costs a D1 read.
//!
//! - `GET /v1/auth/github/authorize` (M2-003e): per `CF-Connecting-IP` (when present) 10 per
//!   minute, since it is unauthenticated and writes a row.
//!
//! The clock is passed in. Expired pairings (after a grace period, so a late poll still sees
//! `expired_token`) and old attempts are purged by [`purge`], from the cron trigger and
//! opportunistically when a pairing starts.

use crate::db::{Db, DbError, int};
use crate::threads::ApiError;

pub const APPROVE_FAILS_PER_MINUTE: i64 = 5;
pub const APPROVE_FAILS_PER_HOUR: i64 = 20;
pub const CODE_PER_IP_PER_MINUTE: i64 = 10;
pub const TOKEN_PER_IP_PER_MINUTE: i64 = 60;
pub const AUTHORIZE_PER_IP_PER_MINUTE: i64 = 10;
/// Live (pending, unexpired) pairings the hub holds at once.
pub const MAX_PENDING: i64 = 1000;

const MINUTE_MS: i64 = 60_000;
const HOUR_MS: i64 = 60 * MINUTE_MS;
/// Expired pairings are kept this long so a late poll still gets `expired_token`.
const PAIRING_GRACE_MS: i64 = HOUR_MS;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("devices: {error}"))
}

/// A `429`; the route turns `retry_after_seconds` into the `Retry-After` header.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Limited {
    pub retry_after_seconds: i64,
}

const RETRY_PREFIX: &str = "too many attempts; retry in ";

impl Limited {
    pub fn into_api(self) -> ApiError {
        ApiError::new(
            429,
            "rate_limited",
            format!("{RETRY_PREFIX}{} seconds", self.retry_after_seconds),
        )
    }
}

/// The `Retry-After` seconds of a [`Limited::into_api`] error (`None` for any other error).
pub fn retry_after(error: &ApiError) -> Option<i64> {
    if error.status != 429 {
        return None;
    }
    error
        .message
        .strip_prefix(RETRY_PREFIX)?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn seconds_until(free_at_ms: i64, now_ms: i64) -> i64 {
    (((free_at_ms - now_ms).max(0) + 999) / 1000).max(1)
}

fn longer(a: Option<Limited>, b: Option<Limited>) -> Option<Limited> {
    match (a, b) {
        (Some(a), Some(b)) => Some(if b.retry_after_seconds > a.retry_after_seconds {
            b
        } else {
            a
        }),
        (a, b) => a.or(b),
    }
}

/// `Some` when `scope` already has `limit` attempts inside `window_ms`: the time until the
/// oldest counted one leaves the window.
async fn over(
    db: &impl Db,
    scope: &str,
    now_ms: i64,
    limit: i64,
    window_ms: i64,
) -> Result<Option<Limited>, DbError> {
    let rows = db
        .query(
            "SELECT at_ms FROM device_attempts WHERE scope = ? AND at_ms > ? \
             ORDER BY at_ms DESC LIMIT 1 OFFSET ?",
            &[
                scope.into(),
                (now_ms - window_ms).into(),
                (limit - 1).into(),
            ],
        )
        .await?;
    let Some(row) = rows.first() else {
        return Ok(None);
    };
    Ok(Some(Limited {
        retry_after_seconds: seconds_until(int(row, "at_ms")? + window_ms, now_ms),
    }))
}

async fn record(db: &impl Db, scope: &str, now_ms: i64) -> Result<(), ApiError> {
    db.execute(
        "INSERT INTO device_attempts (scope, at_ms) VALUES (?, ?)",
        &[scope.into(), now_ms.into()],
    )
    .await
    .map(drop)
    .map_err(db_failure)
}

fn approve_scopes(user_id: &str, org_id: &str) -> [String; 2] {
    [
        format!("approve:user:{user_id}"),
        format!("approve:org:{org_id}"),
    ]
}

/// `Some` when the caller's user or org already failed too many user-code attempts.
pub async fn check_approve(
    db: &impl Db,
    user_id: &str,
    org_id: &str,
    now_ms: i64,
) -> Result<Option<Limited>, ApiError> {
    let mut worst = None;
    for scope in approve_scopes(user_id, org_id) {
        let minute = over(db, &scope, now_ms, APPROVE_FAILS_PER_MINUTE, MINUTE_MS)
            .await
            .map_err(db_failure)?;
        let hour = over(db, &scope, now_ms, APPROVE_FAILS_PER_HOUR, HOUR_MS)
            .await
            .map_err(db_failure)?;
        worst = longer(worst, longer(minute, hour));
    }
    Ok(worst)
}

/// Counts one failed user-code attempt against the user and the org.
pub async fn record_approve_failure(
    db: &impl Db,
    user_id: &str,
    org_id: &str,
    now_ms: i64,
) -> Result<(), ApiError> {
    for scope in approve_scopes(user_id, org_id) {
        record(db, &scope, now_ms).await?;
    }
    Ok(())
}

/// Gate for `POST /v1/devices/token`: `Some` when `client_ip` already polled
/// [`TOKEN_PER_IP_PER_MINUTE`] times in the last minute; an allowed call is counted. No IP (local
/// dev, no Cloudflare in front) means no limit.
pub async fn check_token(
    db: &impl Db,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Option<Limited>, ApiError> {
    let Some(ip) = client_ip.map(str::trim).filter(|ip| !ip.is_empty()) else {
        return Ok(None);
    };
    let scope = format!("token:ip:{}", ip.chars().take(64).collect::<String>());
    if let Some(hit) = over(db, &scope, now_ms, TOKEN_PER_IP_PER_MINUTE, MINUTE_MS)
        .await
        .map_err(db_failure)?
    {
        return Ok(Some(hit));
    }
    record(db, &scope, now_ms).await?;
    Ok(None)
}

/// Gate for `GET /v1/auth/github/authorize`: `Some` when `client_ip` already started
/// [`AUTHORIZE_PER_IP_PER_MINUTE`] sign-ins in the last minute; an allowed call is counted. No IP
/// (local dev, no Cloudflare in front) means no limit.
pub async fn check_authorize(
    db: &impl Db,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Option<Limited>, ApiError> {
    let Some(ip) = client_ip.map(str::trim).filter(|ip| !ip.is_empty()) else {
        return Ok(None);
    };
    let scope = format!("authorize:ip:{}", ip.chars().take(64).collect::<String>());
    if let Some(hit) = over(db, &scope, now_ms, AUTHORIZE_PER_IP_PER_MINUTE, MINUTE_MS)
        .await
        .map_err(db_failure)?
    {
        return Ok(Some(hit));
    }
    record(db, &scope, now_ms).await?;
    Ok(None)
}

/// Gate for `POST /v1/devices/code`: the per-IP window (when the proxy gave an IP) and the cap
/// on live pairings. An allowed call is counted against the IP.
pub async fn check_start(
    db: &impl Db,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Option<Limited>, ApiError> {
    check_start_in(db, "device_pairings", client_ip, now_ms).await
}

/// [`check_start`] for a pairing table: `device_pairings` (machines) or `browser_pairings`
/// (browser sign-in, M2-003c). The per-IP window is shared by both; the cap is per table.
pub async fn check_start_in(
    db: &impl Db,
    table: &'static str,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Option<Limited>, ApiError> {
    debug_assert!(matches!(table, "device_pairings" | "browser_pairings"));
    let scope = client_ip
        .map(str::trim)
        .filter(|ip| !ip.is_empty())
        .map(|ip| format!("code:ip:{}", ip.chars().take(64).collect::<String>()));
    if let Some(scope) = &scope
        && let Some(hit) = over(db, scope, now_ms, CODE_PER_IP_PER_MINUTE, MINUTE_MS)
            .await
            .map_err(db_failure)?
    {
        return Ok(Some(hit));
    }
    let live = db
        .query(
            &format!(
                "SELECT COUNT(*) AS n, MIN(expires_at_ms) AS first_free FROM {table} \
                 WHERE status = 'pending' AND expires_at_ms > ?"
            ),
            &[now_ms.into()],
        )
        .await
        .map_err(db_failure)?;
    if let Some(row) = live.first()
        && int(row, "n").map_err(db_failure)? >= MAX_PENDING
    {
        let free_at = row
            .get("first_free")
            .and_then(serde_json::Value::as_i64)
            .unwrap_or(now_ms + MINUTE_MS);
        return Ok(Some(Limited {
            retry_after_seconds: seconds_until(free_at, now_ms),
        }));
    }
    if let Some(scope) = &scope {
        record(db, scope, now_ms).await?;
    }
    Ok(None)
}

/// Deletes pairings that expired more than an hour ago and attempts older than the longest
/// window. Returns `(pairings, attempts)` deleted.
pub async fn purge(db: &impl Db, now_ms: i64) -> Result<(u64, u64), ApiError> {
    let pairings = db
        .execute(
            "DELETE FROM device_pairings WHERE expires_at_ms <= ?",
            &[(now_ms - PAIRING_GRACE_MS).into()],
        )
        .await
        .map_err(db_failure)?;
    // Browser sign-in (M2-003c): its pairings and the spent or expired link codes.
    let browser = db
        .execute(
            "DELETE FROM browser_pairings WHERE expires_at_ms <= ?",
            &[(now_ms - PAIRING_GRACE_MS).into()],
        )
        .await
        .map_err(db_failure)?;
    db.execute(
        "DELETE FROM signin_links WHERE expires_at_ms <= ?",
        &[(now_ms - PAIRING_GRACE_MS).into()],
    )
    .await
    .map_err(db_failure)?;
    let attempts = db
        .execute(
            "DELETE FROM device_attempts WHERE at_ms <= ?",
            &[(now_ms - HOUR_MS).into()],
        )
        .await
        .map_err(db_failure)?;
    Ok((pairings + browser, attempts))
}
