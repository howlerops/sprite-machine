//! The machine admin API over the `Db` trait (M2B-002): create a machine with a one-time
//! credential, revoke it, list the org's machines. Platform-neutral; `routes.rs` does HTTP.
//!
//! Every mutation needs the admin role and writes an audit row first (plan §4.2 "Auth"). The
//! credential (`smm_` + 256 bits) is generated here, shown once in the create answer, and only
//! its SHA-256 goes to D1 and to the Machine DO (via [`MachineControl`]).

use std::future::Future;

use serde_json::{Value, json};

use super::credential::{hash, new_credential, new_machine_id};
use crate::audit::{self, Event};
use crate::auth::Principal;
use crate::db::{Db, DbError, Param, Role, Row, int, opt_text, text};
use crate::threads::ApiError;

const MAX_NAME_CHARS: usize = 128;

const NOW: &str = "strftime('%Y-%m-%dT%H:%M:%fZ', 'now')";

/// What the admin API needs from the Machine DO.
pub trait MachineControl {
    /// Stores the machine's identity and credential hash in its Durable Object.
    fn provision(
        &self,
        machine_id: &str,
        org_id: &str,
        credential_hash: &str,
    ) -> impl Future<Output = Result<(), ApiError>>;

    /// Revokes it: the live tunnel closes with `auth_revoked`, the credential stops working.
    fn revoke(&self, machine_id: &str) -> impl Future<Output = Result<(), ApiError>>;

    /// Closes the live tunnel with `device_untrusted` without revoking: the machine may reconnect.
    fn disconnect(&self, machine_id: &str) -> impl Future<Output = Result<(), ApiError>>;
}

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("machines: {error}"))
}

/// A `machines` row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    pub id: String,
    pub org_id: String,
    pub name: String,
    pub kind: String,
    pub created_at: String,
    pub revoked_at: Option<String>,
    pub last_seen_at: Option<String>,
    pub presence: String,
    pub presence_since: Option<String>,
    /// The runner's capabilities as its last `hello` sent them (JSON text), if it has said hello.
    pub capabilities: Option<String>,
}

impl MachineRow {
    fn from_row(row: &Row) -> Result<Self, DbError> {
        Ok(Self {
            id: text(row, "id")?,
            org_id: text(row, "org_id")?,
            name: text(row, "name")?,
            kind: text(row, "kind")?,
            created_at: text(row, "created_at")?,
            revoked_at: opt_text(row, "revoked_at")?,
            last_seen_at: opt_text(row, "last_seen_at")?,
            presence: text(row, "presence")?,
            presence_since: opt_text(row, "presence_since")?,
            capabilities: opt_text(row, "capabilities")?,
        })
    }

    /// The API shape (never includes the credential or its hash).
    pub fn to_json(&self) -> Value {
        json!({
            "id": self.id,
            "name": self.name,
            "kind": self.kind,
            "created_at": self.created_at,
            "revoked_at": self.revoked_at,
            "last_seen_at": self.last_seen_at,
            "presence": self.presence,
            "presence_since": self.presence_since,
        })
    }

    /// The client-api §5.1 shape of one machine in `GET /v1/machines`. `since` falls back to the
    /// creation time for a machine whose presence was never mirrored (it has been offline since).
    pub fn to_client_json(&self) -> Value {
        json!({
            "machine_id": self.id,
            "name": self.name,
            "kind": self.kind,
            "presence": {
                "state": self.presence,
                "since": self.presence_since.as_deref().unwrap_or(&self.created_at),
            },
            "capabilities": client_capabilities(self.capabilities.as_deref()),
        })
    }
}

/// The §5.1 `capabilities` object: the three fields the client API names, or `null` before the
/// runner's first `hello`.
fn client_capabilities(stored: Option<&str>) -> Value {
    let Some(stored) = stored.and_then(|text| serde_json::from_str::<Value>(text).ok()) else {
        return Value::Null;
    };
    json!({
        "harnesses": stored["harnesses"],
        "spawn": stored["spawn"],
        "max_sessions": stored["max_sessions"],
    })
}

const COLUMNS: &str = "id, org_id, name, kind, created_at, revoked_at, last_seen_at, presence, \
                       presence_since, capabilities";

async fn find(db: &impl Db, org_id: &str, id: &str) -> Result<Option<MachineRow>, ApiError> {
    let rows = db
        .query(
            &format!("SELECT {COLUMNS} FROM machines WHERE id = ? AND org_id = ?"),
            &[id.into(), org_id.into()],
        )
        .await
        .map_err(db_failure)?;
    rows.first()
        .map(MachineRow::from_row)
        .transpose()
        .map_err(db_failure)
}

fn parse_create(body: &Value) -> Result<(String, &'static str), ApiError> {
    let name = match body.get("name") {
        Some(Value::String(name)) => name.trim().to_owned(),
        _ => return Err(ApiError::invalid_request("name is required")),
    };
    if name.is_empty() || name.chars().count() > MAX_NAME_CHARS {
        return Err(ApiError::invalid_request(format!(
            "name must be 1-{MAX_NAME_CHARS} characters"
        )));
    }
    let kind = match body.get("kind") {
        None | Some(Value::Null) => "local",
        Some(Value::String(kind)) if kind == "local" => "local",
        Some(Value::String(kind)) if kind == "sandbox" => "sandbox",
        Some(_) => {
            return Err(ApiError::invalid_request(
                "kind must be \"local\" or \"sandbox\"",
            ));
        }
    };
    Ok((name, kind))
}

/// `POST /v1/machines`: admin. Answers `{machine, credential}`; the credential is not stored
/// anywhere in clear and cannot be read again.
pub async fn create_machine(
    db: &impl Db,
    control: &impl MachineControl,
    caller: &Principal,
    body: &Value,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    let (name, kind) = parse_create(body)?;
    let id = new_machine_id()?;
    let credential = new_credential()?;
    let credential_hash = hash(&credential);

    audit::record(
        db,
        &Event::by(
            caller,
            "machine.create",
            "machine",
            &id,
            request_id,
            json!({ "name": name, "kind": kind }),
        ),
    )
    .await?;
    db.execute(
        "INSERT INTO machines (id, org_id, name, kind, credential_hash) VALUES (?, ?, ?, ?, ?)",
        &[
            id.as_str().into(),
            caller.org_id.as_str().into(),
            name.as_str().into(),
            kind.into(),
            credential_hash.as_str().into(),
        ],
    )
    .await
    .map_err(db_failure)?;
    if let Err(error) = control
        .provision(&id, &caller.org_id, &credential_hash)
        .await
    {
        // Without its object the machine could never connect: do not leave the row behind.
        let _ = db
            .execute("DELETE FROM machines WHERE id = ?", &[id.as_str().into()])
            .await;
        return Err(error);
    }
    let row = find(db, &caller.org_id, &id)
        .await?
        .ok_or_else(|| ApiError::internal("machine row vanished"))?;
    Ok(json!({ "machine": row.to_json(), "credential": credential }))
}

/// `DELETE /v1/machines/{id}`: admin. Idempotent; closes the live tunnel with `auth_revoked`.
pub async fn revoke_machine(
    db: &impl Db,
    control: &impl MachineControl,
    caller: &Principal,
    machine_id: &str,
    request_id: &str,
) -> Result<Value, ApiError> {
    caller.require(Role::Admin)?;
    let Some(row) = find(db, &caller.org_id, machine_id).await? else {
        return Err(ApiError::not_found(format!("no machine {machine_id}")));
    };
    audit::record(
        db,
        &Event::by(
            caller,
            "machine.revoke",
            "machine",
            machine_id,
            request_id,
            json!({ "name": row.name }),
        ),
    )
    .await?;
    db.execute(
        &format!(
            "UPDATE machines SET revoked_at = {NOW}, presence = 'offline', presence_since = {NOW} \
             WHERE id = ? AND org_id = ? AND revoked_at IS NULL"
        ),
        &[machine_id.into(), caller.org_id.as_str().into()],
    )
    .await
    .map_err(db_failure)?;
    // Always tell the object, so a revoke whose first attempt failed after the row update is
    // finished by the retry.
    control.revoke(machine_id).await?;
    let row = find(db, &caller.org_id, machine_id)
        .await?
        .ok_or_else(|| ApiError::internal("machine row vanished"))?;
    Ok(json!({ "id": row.id, "revoked_at": row.revoked_at }))
}

/// `GET /v1/machines`: viewer. The org's machines in the client-api §5.1 shape, newest first.
/// Revoked machines are left out (they cannot connect, and §5.1 has no field for them).
pub async fn list_machines(db: &impl Db, caller: &Principal) -> Result<Value, ApiError> {
    caller.require(Role::Viewer)?;
    let rows = db
        .query(
            &format!(
                "SELECT {COLUMNS} FROM machines WHERE org_id = ? AND revoked_at IS NULL \
                 ORDER BY created_at DESC, id"
            ),
            &[caller.org_id.as_str().into()],
        )
        .await
        .map_err(db_failure)?;
    let machines = rows
        .iter()
        .map(|row| MachineRow::from_row(row).map(|m| m.to_client_json()))
        .collect::<Result<Vec<_>, _>>()
        .map_err(db_failure)?;
    Ok(json!({ "machines": machines }))
}

/// Mirrors the Machine DO's presence into D1 (best effort; the DO is the source of truth).
pub async fn record_presence(
    db: &impl Db,
    machine_id: &str,
    state: &str,
    since: &str,
) -> Result<(), DbError> {
    db.execute(
        &format!(
            "UPDATE machines SET presence = ?, presence_since = ?, last_seen_at = {NOW} \
             WHERE id = ?"
        ),
        &[state.into(), since.into(), machine_id.into()],
    )
    .await
    .map(drop)
}

/// Mirrors the runner's capabilities from its `hello` into D1 (best effort; the DO is the source
/// of truth).
pub async fn record_capabilities(
    db: &impl Db,
    machine_id: &str,
    capabilities: &Value,
) -> Result<(), DbError> {
    db.execute(
        "UPDATE machines SET capabilities = ? WHERE id = ?",
        &[capabilities.to_string().into(), machine_id.into()],
    )
    .await
    .map(drop)
}

/// The number of machines in an org (used by tests).
pub async fn count(db: &impl Db, org_id: &str) -> Result<i64, DbError> {
    let rows = db
        .query(
            "SELECT COUNT(*) AS n FROM machines WHERE org_id = ?",
            &[Param::Text(org_id.to_owned())],
        )
        .await?;
    rows.first().map_or(Ok(0), |row| int(row, "n"))
}
