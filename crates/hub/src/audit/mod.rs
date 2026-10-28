//! The audit log writer (plan §4.2 "Auth": every mutation is audited).
//!
//! Rows go into `audit_log` (append-only by trigger). The caller names the actor (always the
//! authenticated [`Principal`], never a request body), the action, the target and the request
//! id; `detail` is a JSON object that [`redact`] scrubs of anything that looks like a secret
//! before it is stored.

use serde_json::{Map, Value};

use crate::auth::Principal;
use crate::auth::token::{TOKEN_PREFIX, new_id};
use crate::db::{Db, DbError, Param, Row, int, opt_text, text};
use crate::threads::ApiError;

/// Key fragments that mark a value as secret; matching keys are replaced, case-insensitively.
const SECRET_KEYS: &[&str] = &[
    "token",
    "secret",
    "password",
    "authorization",
    "credential",
    "api_key",
    "apikey",
];

/// Replaces the value of every secret-looking key (at any depth) with `"[redacted]"`, and any
/// string that contains a session token prefix.
pub fn redact(value: &Value) -> Value {
    match value {
        Value::Object(map) => {
            let mut out = Map::new();
            for (key, inner) in map {
                let lower = key.to_ascii_lowercase();
                if SECRET_KEYS.iter().any(|needle| lower.contains(needle)) {
                    out.insert(key.clone(), Value::String("[redacted]".to_owned()));
                } else {
                    out.insert(key.clone(), redact(inner));
                }
            }
            Value::Object(out)
        }
        Value::Array(items) => Value::Array(items.iter().map(redact).collect()),
        Value::String(text) if text.contains(TOKEN_PREFIX) => {
            Value::String("[redacted]".to_owned())
        }
        other => other.clone(),
    }
}

/// One audit event.
#[derive(Debug, Clone)]
pub struct Event<'a> {
    pub org_id: &'a str,
    pub actor_user_id: &'a str,
    pub actor_device_id: &'a str,
    /// Dotted verb, e.g. `thread.prompt`, `auth.session.issue`.
    pub action: &'a str,
    pub target_kind: &'a str,
    pub target_id: &'a str,
    pub request_id: &'a str,
    pub detail: Value,
}

impl<'a> Event<'a> {
    /// An event by `principal` (org, user and device come from the session).
    pub fn by(
        principal: &'a Principal,
        action: &'a str,
        target_kind: &'a str,
        target_id: &'a str,
        request_id: &'a str,
        detail: Value,
    ) -> Self {
        Self {
            org_id: &principal.org_id,
            actor_user_id: &principal.user_id,
            actor_device_id: &principal.device_id,
            action,
            target_kind,
            target_id,
            request_id,
            detail,
        }
    }
}

/// Appends one audit row. The detail is redacted first.
pub async fn record(db: &impl Db, event: &Event<'_>) -> Result<(), ApiError> {
    let id = new_id("aud")?;
    let detail = serde_json::to_string(&redact(&event.detail))
        .map_err(|error| ApiError::internal(format!("audit detail: {error}")))?;
    db.execute(
        "INSERT INTO audit_log (id, org_id, actor_user_id, actor_device_id, action, target_kind, \
         target_id, request_id, detail_json) VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
        &[
            id.into(),
            event.org_id.into(),
            event.actor_user_id.into(),
            event.actor_device_id.into(),
            event.action.into(),
            event.target_kind.into(),
            event.target_id.into(),
            event.request_id.into(),
            Param::Text(detail),
        ],
    )
    .await
    .map_err(|error| ApiError::internal(format!("audit: {error}")))?;
    Ok(())
}

/// An audit row as the dev read route returns it.
#[derive(Debug, Clone, PartialEq)]
pub struct Entry {
    pub seq: i64,
    pub actor_user_id: Option<String>,
    pub actor_device_id: Option<String>,
    pub action: String,
    pub target_kind: String,
    pub target_id: String,
    pub request_id: String,
    pub detail: Value,
    pub created_at: String,
}

impl Entry {
    pub fn to_json(&self) -> Value {
        serde_json::json!({
            "seq": self.seq,
            "actor_user_id": self.actor_user_id,
            "actor_device_id": self.actor_device_id,
            "action": self.action,
            "target_kind": self.target_kind,
            "target_id": self.target_id,
            "request_id": self.request_id,
            "detail": self.detail,
            "created_at": self.created_at,
        })
    }
}

/// An org's audit rows after `after_seq`, oldest first, at most `limit`.
pub async fn list(
    db: &impl Db,
    org_id: &str,
    after_seq: i64,
    limit: i64,
) -> Result<Vec<Entry>, ApiError> {
    let rows = db
        .query(
            "SELECT seq, actor_user_id, actor_device_id, action, target_kind, target_id, \
             request_id, detail_json, created_at FROM audit_log \
             WHERE org_id = ? AND seq > ? ORDER BY seq LIMIT ?",
            &[org_id.into(), after_seq.into(), limit.into()],
        )
        .await
        .map_err(|error| ApiError::internal(format!("audit: {error}")))?;
    rows.iter()
        .map(entry_from_row)
        .collect::<Result<_, _>>()
        .map_err(|error| ApiError::internal(format!("audit: {error}")))
}

fn entry_from_row(row: &Row) -> Result<Entry, DbError> {
    let detail_text = text(row, "detail_json")?;
    Ok(Entry {
        seq: int(row, "seq")?,
        actor_user_id: opt_text(row, "actor_user_id")?,
        actor_device_id: opt_text(row, "actor_device_id")?,
        action: text(row, "action")?,
        target_kind: text(row, "target_kind")?,
        target_id: text(row, "target_id")?,
        request_id: text(row, "request_id")?,
        detail: serde_json::from_str(&detail_text)
            .map_err(|error| DbError::Invalid(format!("audit detail: {error}")))?,
        created_at: text(row, "created_at")?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn redact_hides_secret_keys_and_token_strings() {
        let out = redact(&json!({
            "thread_id": "t1",
            "Authorization": "Bearer abc",
            "nested": { "session_token": "x", "ok": 1 },
            "note": "see smt_abcdef",
        }));
        assert_eq!(out["thread_id"], "t1");
        assert_eq!(out["Authorization"], "[redacted]");
        assert_eq!(out["nested"]["session_token"], "[redacted]");
        assert_eq!(out["nested"]["ok"], 1);
        assert_eq!(out["note"], "[redacted]");
    }
}
