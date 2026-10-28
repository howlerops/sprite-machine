//! The device-pairing endpoints over the `Db` trait (M2B-005). Platform-neutral; `routes.rs`
//! does HTTP. The clock (`now_ms`, epoch milliseconds) is passed in.
//!
//! Flow (RFC 8628): [`start`] -> a member runs [`approve`] (or [`deny`]) with the user code ->
//! the runner calls [`poll`] with its device code until it gets the machine id and credential,
//! once. The credential is minted at that moment, never stored in clear, and the pairing is
//! marked `consumed` first so two racing polls cannot both receive one.

use serde_json::{Value, json};

use super::codes::{
    display_user_code, hash_device_code, new_device_code, new_user_code, normalize_user_code,
};
use super::limits;
use crate::audit::{self, Event};
use crate::auth::Principal;
use crate::auth::token::new_id;
use crate::db::{Db, DbError, Param, Role, Row, int, opt_text, text};
use crate::machines::admin::{self, MachineControl};
use crate::machines::credential::{hash, new_credential, new_machine_id};
use crate::threads::ApiError;

/// A pairing lives 10 minutes (RFC 8628 `expires_in`).
pub const EXPIRES_IN_SECONDS: i64 = 600;
/// The default poll interval, and how much `slow_down` adds (RFC 8628 §3.5).
pub const INTERVAL_SECONDS: i64 = 5;
pub const SLOW_DOWN_STEP_SECONDS: i64 = 5;
const MAX_NAME_CHARS: usize = 128;

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("devices: {error}"))
}

/// What `POST /v1/devices/token` answers when there is no credential yet: an RFC 8628 §3.5
/// error. `interval` is set on `slow_down` (the new, larger interval).
#[derive(Debug, Clone, PartialEq)]
pub enum PollError {
    Rfc {
        code: &'static str,
        interval: Option<i64>,
    },
    Api(ApiError),
}

impl PollError {
    fn rfc(code: &'static str) -> Self {
        Self::Rfc {
            code,
            interval: None,
        }
    }

    /// `(status, body)`: RFC errors are `400 {"error": code, ...}`.
    pub fn response(&self) -> (u16, Value) {
        match self {
            Self::Rfc { code, interval } => {
                let mut body = json!({ "error": code });
                if let Some(interval) = interval {
                    body["interval"] = json!(interval);
                }
                (400, body)
            }
            Self::Api(error) => (error.status, error.body()),
        }
    }
}

impl From<ApiError> for PollError {
    fn from(error: ApiError) -> Self {
        Self::Api(error)
    }
}

/// `POST /v1/devices/code`: unauthenticated. `base_url` is the hub's public origin.
///
/// Open to the world, so it is limited (`limits::check_start`: per `client_ip` when known, and a
/// cap on live pairings; `429`) and it purges long-expired rows as it goes.
pub async fn start(
    db: &impl Db,
    base_url: &str,
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<Value, ApiError> {
    // Best effort: a failed purge must not stop a pairing (the cron trigger purges too).
    let _ = limits::purge(db, now_ms).await;
    if let Some(limited) = limits::check_start(db, client_ip, now_ms).await? {
        return Err(limited.into_api());
    }
    let device_code = new_device_code()?;
    let id = new_id("dpr")?;
    // A user-code collision (31^8 space) hits the UNIQUE index; retry a few times.
    let mut user_code = String::new();
    let mut inserted = false;
    for _ in 0..5 {
        user_code = new_user_code()?;
        let result = db
            .execute(
                "INSERT INTO device_pairings (id, device_code_hash, user_code, created_at_ms, \
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
        "verification_uri": format!("{base}/device"),
        "verification_uri_complete": format!("{base}/device?user_code={shown}"),
        "expires_in": EXPIRES_IN_SECONDS,
        "interval": INTERVAL_SECONDS,
    }))
}

pub(super) fn user_code_of(body: &Value) -> Result<String, ApiError> {
    body.get("user_code")
        .and_then(Value::as_str)
        .and_then(normalize_user_code)
        .ok_or_else(|| ApiError::invalid_request("user_code must look like ABCD-EFGH"))
}

fn machine_name_of(body: &Value) -> Result<String, ApiError> {
    let name = match body.get("name") {
        Some(Value::String(name)) => name.trim().to_owned(),
        _ => return Err(ApiError::invalid_request("name is required")),
    };
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::invalid_request(format!(
            "name must be 1-{MAX_NAME_CHARS} characters"
        )));
    }
    Ok(name)
}

/// Moves a live `pending` pairing to `to`; `404` when there is none to move.
async fn resolve_pending(
    db: &impl Db,
    user_code: &str,
    now_ms: i64,
    set: &str,
    params: Vec<Param>,
) -> Result<String, ApiError> {
    let mut all = params;
    all.push(user_code.into());
    all.push(now_ms.into());
    let changed = db
        .execute(
            &format!(
                "UPDATE device_pairings SET {set} WHERE user_code = ? AND status = 'pending' \
                 AND expires_at_ms > ?"
            ),
            &all,
        )
        .await
        .map_err(db_failure)?;
    if changed == 0 {
        return Err(ApiError::not_found(
            "no pending pairing with that code (unknown, expired or already answered)",
        ));
    }
    let rows = db
        .query(
            "SELECT id FROM device_pairings WHERE user_code = ?",
            &[user_code.into()],
        )
        .await
        .map_err(db_failure)?;
    rows.first()
        .map(|row| text(row, "id"))
        .transpose()
        .map_err(db_failure)?
        .ok_or_else(|| ApiError::internal("pairing row vanished"))
}

/// [`resolve_pending`] behind the user-code brute-force limit (RFC 8628 §5.1): refused with
/// `429` once the caller's user or org has too many failed attempts; a miss counts as one. A hit
/// writes nothing to the counters, so it resets nobody's.
async fn resolve_limited(
    db: &impl Db,
    caller: &Principal,
    user_code: &str,
    now_ms: i64,
    set: &str,
    params: Vec<Param>,
) -> Result<String, ApiError> {
    if let Some(limited) =
        limits::check_approve(db, &caller.user_id, &caller.org_id, now_ms).await?
    {
        return Err(limited.into_api());
    }
    let result = resolve_pending(db, user_code, now_ms, set, params).await;
    if matches!(&result, Err(error) if error.status == 404) {
        limits::record_approve_failure(db, &caller.user_id, &caller.org_id, now_ms).await?;
    }
    result
}

/// `POST /v1/devices/approve {user_code, name}`: member. Approves the pairing for the caller's
/// org; the runner's next poll receives the machine.
pub async fn approve(
    db: &impl Db,
    caller: &Principal,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Member)?;
    let user_code = user_code_of(body)?;
    let name = machine_name_of(body)?;
    let id = resolve_limited(
        db,
        caller,
        &user_code,
        now_ms,
        "status = 'approved', org_id = ?, approved_by_user_id = ?, approved_by_device_id = ?, \
         machine_name = ?",
        vec![
            caller.org_id.as_str().into(),
            caller.user_id.as_str().into(),
            caller.device_id.as_str().into(),
            name.as_str().into(),
        ],
    )
    .await?;
    audit::record(
        db,
        &Event::by(
            caller,
            "device.approve",
            "device_pairing",
            &id,
            request_id,
            json!({ "name": name }),
        ),
    )
    .await?;
    Ok(json!({ "status": "approved", "name": name }))
}

/// `POST /v1/devices/deny {user_code}`: member. The runner's next poll gets `access_denied`.
pub async fn deny(
    db: &impl Db,
    caller: &Principal,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Member)?;
    let user_code = user_code_of(body)?;
    let id = resolve_limited(
        db,
        caller,
        &user_code,
        now_ms,
        "status = 'denied', org_id = ?, approved_by_user_id = ?, approved_by_device_id = ?",
        vec![
            caller.org_id.as_str().into(),
            caller.user_id.as_str().into(),
            caller.device_id.as_str().into(),
        ],
    )
    .await?;
    audit::record(
        db,
        &Event::by(
            caller,
            "device.deny",
            "device_pairing",
            &id,
            request_id,
            json!({}),
        ),
    )
    .await?;
    Ok(json!({ "status": "denied" }))
}

struct Pairing {
    id: String,
    status: String,
    expires_at_ms: i64,
    interval_seconds: i64,
    last_poll_at_ms: Option<i64>,
    org_id: Option<String>,
    approved_by_user_id: Option<String>,
    approved_by_device_id: Option<String>,
    machine_name: Option<String>,
}

impl Pairing {
    fn from_row(row: &Row) -> Result<Self, DbError> {
        Ok(Self {
            id: text(row, "id")?,
            status: text(row, "status")?,
            expires_at_ms: int(row, "expires_at_ms")?,
            interval_seconds: int(row, "interval_seconds")?,
            last_poll_at_ms: row.get("last_poll_at_ms").and_then(Value::as_i64),
            org_id: opt_text(row, "org_id")?,
            approved_by_user_id: opt_text(row, "approved_by_user_id")?,
            approved_by_device_id: opt_text(row, "approved_by_device_id")?,
            machine_name: opt_text(row, "machine_name")?,
        })
    }
}

/// `POST /v1/devices/token {device_code}`: unauthenticated (the device code is the secret).
/// `Ok` carries the machine id and credential, exactly once.
pub async fn poll(
    db: &impl Db,
    control: &impl MachineControl,
    device_code: &str,
    now_ms: i64,
) -> Result<Value, PollError> {
    let rows = db
        .query(
            "SELECT id, status, expires_at_ms, interval_seconds, last_poll_at_ms, org_id, \
             approved_by_user_id, approved_by_device_id, machine_name \
             FROM device_pairings WHERE device_code_hash = ?",
            &[hash_device_code(device_code).into()],
        )
        .await
        .map_err(db_failure)?;
    let pairing = rows
        .first()
        .map(Pairing::from_row)
        .transpose()
        .map_err(db_failure)?
        .ok_or_else(|| PollError::rfc("invalid_grant"))?;

    if pairing.status == "consumed" {
        return Err(PollError::rfc("invalid_grant"));
    }
    if pairing.expires_at_ms <= now_ms {
        return Err(PollError::rfc("expired_token"));
    }
    if pairing.status == "denied" {
        return Err(PollError::rfc("access_denied"));
    }

    // Polling faster than the interval: slow_down, and the interval grows (RFC 8628 §3.5).
    let too_fast = pairing
        .last_poll_at_ms
        .is_some_and(|last| now_ms - last < pairing.interval_seconds * 1000);
    let interval = if too_fast {
        pairing.interval_seconds + SLOW_DOWN_STEP_SECONDS
    } else {
        pairing.interval_seconds
    };
    db.execute(
        "UPDATE device_pairings SET last_poll_at_ms = ?, interval_seconds = ? WHERE id = ?",
        &[now_ms.into(), interval.into(), pairing.id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    if too_fast {
        return Err(PollError::Rfc {
            code: "slow_down",
            interval: Some(interval),
        });
    }
    if pairing.status == "pending" {
        return Err(PollError::rfc("authorization_pending"));
    }
    Ok(grant(db, control, &pairing).await?)
}

/// Hands out the machine of an approved pairing: claim it (`approved` -> `consumed`), create the
/// machine row like `POST /v1/machines` does, provision the Machine DO, answer once.
async fn grant(
    db: &impl Db,
    control: &impl MachineControl,
    pairing: &Pairing,
) -> Result<Value, ApiError> {
    let (Some(org_id), Some(user_id), Some(device_id), Some(name)) = (
        pairing.org_id.as_deref(),
        pairing.approved_by_user_id.as_deref(),
        pairing.approved_by_device_id.as_deref(),
        pairing.machine_name.as_deref(),
    ) else {
        return Err(ApiError::internal("approved pairing is incomplete"));
    };
    let claimed = db
        .execute(
            "UPDATE device_pairings SET status = 'consumed' WHERE id = ? AND status = 'approved'",
            &[pairing.id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    if claimed == 0 {
        // Lost the race to another poll: the credential went to that one.
        return Err(ApiError::new(
            400,
            "invalid_grant",
            "this pairing was already used",
        ));
    }
    let release = || async {
        let _ = db
            .execute(
                "UPDATE device_pairings SET status = 'approved' WHERE id = ?",
                &[pairing.id.as_str().into()],
            )
            .await;
    };

    let machine_id = new_machine_id()?;
    let credential = new_credential()?;
    let credential_hash = hash(&credential);
    let inserted = db
        .execute(
            "INSERT INTO machines (id, org_id, name, kind, credential_hash, origin, \
             approved_by_user_id, approved_at) \
             VALUES (?, ?, ?, 'local', ?, 'device', ?, strftime('%Y-%m-%dT%H:%M:%fZ', 'now'))",
            &[
                machine_id.as_str().into(),
                org_id.into(),
                name.into(),
                credential_hash.as_str().into(),
                user_id.into(),
            ],
        )
        .await;
    if let Err(error) = inserted {
        release().await;
        return Err(db_failure(error));
    }
    if let Err(error) = control
        .provision(&machine_id, org_id, &credential_hash)
        .await
    {
        let _ = db
            .execute(
                "DELETE FROM machines WHERE id = ?",
                &[machine_id.as_str().into()],
            )
            .await;
        release().await;
        return Err(error);
    }
    db.execute(
        "UPDATE device_pairings SET machine_id = ? WHERE id = ?",
        &[machine_id.as_str().into(), pairing.id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    // The approver is the actor: the runner has no session.
    audit::record(
        db,
        &Event {
            org_id,
            actor_user_id: user_id,
            actor_device_id: device_id,
            action: "device.pair",
            target_kind: "machine",
            target_id: &machine_id,
            request_id: &pairing.id,
            detail: json!({ "name": name }),
        },
    )
    .await?;
    // Who vouched for this machine, so the runner can show it before keeping the credential
    // (RFC 8628 §5.4: a victim must not join an attacker's org unknowingly).
    let who = db
        .query(
            "SELECT o.name AS org_name, u.display_name AS user_name, u.email AS user_email \
             FROM orgs o, users u WHERE o.id = ? AND u.id = ?",
            &[org_id.into(), user_id.into()],
        )
        .await
        .map_err(db_failure)?;
    let who = who.first();
    let field = |column: &str| {
        who.and_then(|row| opt_text(row, column).ok().flatten())
            .unwrap_or_default()
    };
    Ok(json!({
        "machine_id": machine_id,
        "credential": credential,
        "org_id": org_id,
        "name": name,
        "org": { "id": org_id, "name": field("org_name") },
        "approved_by": { "id": user_id, "name": field("user_name"), "email": field("user_email") },
    }))
}

/// `DELETE /v1/devices/self`: authenticated by the machine's own credential (`Bearer smm_...`),
/// no session. A runner whose person declined "Join org X approved by Y?" calls it so the org
/// that approved the pairing loses the machine: the credential stops working at once.
pub async fn revoke_self(
    db: &impl Db,
    control: &impl MachineControl,
    authorization: Option<&str>,
    request_id: &str,
) -> Result<Value, ApiError> {
    let credential = crate::auth::bearer_token(authorization)
        .ok_or_else(|| ApiError::unauthorized("missing machine credential"))?;
    let rows = db
        .query(
            "SELECT m.id AS id, m.org_id AS org_id, m.name AS name, \
             p.approved_by_user_id AS user_id, p.approved_by_device_id AS device_id \
             FROM machines m JOIN device_pairings p ON p.machine_id = m.id \
             WHERE m.credential_hash = ? AND m.origin = 'device' AND m.revoked_at IS NULL",
            &[hash(credential).into()],
        )
        .await
        .map_err(db_failure)?;
    let Some(row) = rows.first() else {
        return Err(ApiError::unauthorized("invalid machine credential"));
    };
    let machine_id = text(row, "id").map_err(db_failure)?;
    let org_id = text(row, "org_id").map_err(db_failure)?;
    let user_id = text(row, "user_id").map_err(db_failure)?;
    let device_id = text(row, "device_id").map_err(db_failure)?;
    let name = text(row, "name").map_err(db_failure)?;
    audit::record(
        db,
        &Event {
            org_id: &org_id,
            actor_user_id: &user_id,
            actor_device_id: &device_id,
            action: "device.decline",
            target_kind: "machine",
            target_id: &machine_id,
            request_id,
            detail: json!({ "name": name }),
        },
    )
    .await?;
    db.execute(
        "UPDATE machines SET revoked_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now'), \
         presence = 'offline', presence_since = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') \
         WHERE id = ? AND revoked_at IS NULL",
        &[machine_id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    control.revoke(&machine_id).await?;
    Ok(json!({ "id": machine_id, "revoked": true }))
}

/// `GET /v1/devices`: viewer. The org's paired devices (machines with `origin = 'device'`),
/// newest first, and the org's `require_trusted_devices` setting.
pub async fn list(db: &impl Db, caller: &Principal) -> Result<Value, ApiError> {
    caller.require(Role::Viewer)?;
    let rows = db
        .query(
            "SELECT id, name, created_at, revoked_at, last_seen_at, presence, \
             approved_by_user_id, approved_at FROM machines \
             WHERE org_id = ? AND origin = 'device' ORDER BY created_at DESC, id",
            &[caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let devices = rows
        .iter()
        .map(|row| {
            Ok(json!({
                "machine_id": text(row, "id")?,
                "name": text(row, "name")?,
                "created_at": text(row, "created_at")?,
                "revoked_at": opt_text(row, "revoked_at")?,
                "last_seen_at": opt_text(row, "last_seen_at")?,
                "presence": text(row, "presence")?,
                "approved_by": opt_text(row, "approved_by_user_id")?,
                "approved_at": opt_text(row, "approved_at")?,
            }))
        })
        .collect::<Result<Vec<_>, DbError>>()
        .map_err(db_failure)?;
    let setting = db
        .query(
            "SELECT require_trusted_devices FROM orgs WHERE id = ?",
            &[caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let require = setting
        .first()
        .map(|row| int(row, "require_trusted_devices"))
        .transpose()
        .map_err(db_failure)?
        == Some(1);
    Ok(json!({ "devices": devices, "require_trusted_devices": require }))
}

/// `DELETE /v1/devices/{machine_id}`: admin. Only paired devices; the machines registry does
/// the revoke (the live tunnel closes with `auth_revoked`).
pub async fn revoke(
    db: &impl Db,
    control: &impl MachineControl,
    caller: &Principal,
    machine_id: &str,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    let rows = db
        .query(
            "SELECT id FROM machines WHERE id = ? AND org_id = ? AND origin = 'device'",
            &[machine_id.into(), caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("no device {machine_id}")));
    }
    admin::revoke_machine(db, control, caller, machine_id, request_id).await
}

/// `PUT /v1/devices/policy {require_trusted_devices: bool}`: admin. See `trust.rs`.
pub async fn set_policy(
    db: &impl Db,
    caller: &Principal,
    body: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    let Some(require) = body.get("require_trusted_devices").and_then(Value::as_bool) else {
        return Err(ApiError::invalid_request(
            "require_trusted_devices must be a boolean",
        ));
    };
    audit::record(
        db,
        &Event::by(
            caller,
            "device.policy",
            "org",
            &caller.org_id,
            request_id,
            json!({ "require_trusted_devices": require }),
        ),
    )
    .await?;
    db.execute(
        "UPDATE orgs SET require_trusted_devices = ?, \
         updated_at = strftime('%Y-%m-%dT%H:%M:%fZ', 'now') WHERE id = ?",
        &[require.into(), caller.org_id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    Ok(json!({ "require_trusted_devices": require }))
}
