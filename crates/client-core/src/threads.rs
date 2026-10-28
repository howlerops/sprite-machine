//! Thread creation: `POST /v1/threads` (client-api.md §5.2).
//!
//! The first prompt is optional: a thread created without one is idle. The caller generates the
//! `Idempotency-Key` once per user intent (§3, [`crate::outbox::new_idempotency_key`]) and sends
//! the same key again on a retry, so the hub replays its answer instead of creating a second
//! thread.

use protocol::tunnel::{ContentPart, Workspace};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::outbox::valid_key;

/// The `POST /v1/threads` body. `prompt` is omitted for an idle thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CreateThread {
    pub machine_id: String,
    pub agent: String,
    pub workspace: Workspace,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_mode: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prompt: Option<FirstPrompt>,
}

/// The first prompt of a new thread. `task_id` and `message_id` are client-chosen.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FirstPrompt {
    pub task_id: String,
    pub message_id: String,
    pub parts: Vec<ContentPart>,
}

/// The 201 answer: `{ thread_id, task_id, status }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ThreadCreated {
    pub thread_id: String,
    #[serde(default)]
    pub task_id: Option<String>,
    pub status: String,
}

/// Why a thread was not created. A refused or failed request may be retried with the same key.
#[derive(Debug, thiserror::Error)]
pub enum CreateThreadError {
    #[error("idempotency key {0:?} is not 1-128 characters of [A-Za-z0-9_:.-]")]
    BadKey(String),
    #[error("transport: {0}")]
    Transport(String),
    /// Any answer other than 201; `body` is the hub's error body.
    #[error("POST /v1/threads answered HTTP {status}: {body}")]
    Refused { status: u16, body: Value },
    #[error("POST /v1/threads answered without a thread_id: {0}")]
    Malformed(Value),
}

/// `POST {api}/threads` with the bearer `token` (when set) and `Idempotency-Key`. `api` is the
/// `…/v1` base URL. Returns the 201 answer.
pub async fn create_thread(
    api: &str,
    token: Option<&str>,
    idempotency_key: &str,
    request: &CreateThread,
) -> Result<ThreadCreated, CreateThreadError> {
    if !valid_key(idempotency_key) {
        return Err(CreateThreadError::BadKey(idempotency_key.to_owned()));
    }
    let mut builder = reqwest::Client::new()
        .post(format!("{}/threads", api.trim_end_matches('/')))
        .header("Idempotency-Key", idempotency_key)
        .json(request);
    if let Some(token) = token {
        builder = builder.bearer_auth(token);
    }
    let response = builder
        .send()
        .await
        .map_err(|error| CreateThreadError::Transport(error.to_string()))?;
    let status = response.status().as_u16();
    let text = response
        .text()
        .await
        .map_err(|error| CreateThreadError::Transport(error.to_string()))?;
    let body: Value = serde_json::from_str(&text).unwrap_or(Value::Null);
    if status != 201 {
        return Err(CreateThreadError::Refused { status, body });
    }
    serde_json::from_value(body.clone()).map_err(|_| CreateThreadError::Malformed(body))
}
