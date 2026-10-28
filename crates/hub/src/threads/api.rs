//! The HTTP surface of one Thread DO as a pure function: [`handle`] maps a parsed request to a
//! status and JSON body over a [`ThreadCore`]. The Worker authenticates and forwards; the DO
//! parses the HTTP request into an [`ApiRequest`], calls [`handle`], broadcasts
//! [`ThreadCore::take_appended`] and writes the answer. Everything here runs natively in tests.
//!
//! Public routes (client-api.md §5; the Worker has already authenticated the caller and put the
//! derived `Actor` in the request):
//!
//! | Route                                             | Notes                              |
//! | ------------------------------------------------- | ---------------------------------- |
//! | `GET  /v1/threads/{id}`                           | head seq and `states`              |
//! | `PATCH /v1/threads/{id}`                          | `{title?, archived?}` (M2-010)     |
//! | `DELETE /v1/threads/{id}`                         | soft delete, `Idempotency-Key` (M2-010) |
//! | `POST /v1/threads/{id}/prompts`                   | `Idempotency-Key`                  |
//! | `GET  /v1/threads/{id}/pending`                   |                                    |
//! | `DELETE /v1/threads/{id}/pending/{task_id}`       | `Idempotency-Key`                  |
//! | `POST /v1/threads/{id}/interrupts/{interrupt_id}` | `Idempotency-Key`                  |
//! | `POST /v1/threads/{id}/cancel`                    | `Idempotency-Key`                  |
//! | `POST /v1/threads/{id}/session`                   | `Idempotency-Key`; mode/config checked against `session` state (§5.5a) |
//! | `POST /v1/dev/threads/{id}/events`                | only with `DEV_FIXTURES=1`         |
//!
//! Internal routes (only reachable through a Durable Object stub; the Worker never exposes
//! them). They are the hooks the Machine DO (M2B-002) and thread creation call (`POST /v1/threads`
//! is forwarded here by the Worker with the thread id it derived from the `Idempotency-Key`):
//!
//! | Route                                             | Body / answer                      |
//! | ------------------------------------------------- | ---------------------------------- |
//! | `POST /internal/threads/{id}/events`              | `{event_id, envelope}` (deduped 24 h), or envelope(s) with `seq: 0` in, stamped out |
//! | `POST /internal/threads/{id}/create`              | `POST /v1/threads` body; creates the thread (`201`) |
//! | `GET  /internal/threads/{id}/commands?limit=N`    | `{commands: [...]}` not yet acked  |
//! | `POST /internal/threads/{id}/commands/ack`        | `{ids: [..]}`                      |
//! | `POST /internal/threads/{id}/init`                | `{machine_id, agent, workspace, permission_mode, title}` |
//!
//! The subscription WebSocket (`GET /v1/threads/{id}/subscribe`) needs the runtime and lives in
//! `do/thread.rs`; its messages come from [`ThreadCore::catch_up`].

use protocol::tunnel::Actor;
use protocol::turn_event::{Envelope, ResolvedOutcome};
use serde::Deserialize;
use serde_json::{Map, Value, json};

use protocol::turn_event::is_valid_id;

use super::core::{
    CreateThreadBody, PromptBody, Reply, SessionBody, ThreadCore, UpdateThreadBody, request_hash,
    valid_idempotency_key,
};
use super::error::ApiError;
use super::store::ThreadStore;

/// Commands returned by one `GET /internal/.../commands` call when `limit` is absent.
const DEFAULT_COMMAND_LIMIT: usize = 100;

/// A parsed request for the Thread DO.
#[derive(Debug, Clone)]
pub struct ApiRequest {
    /// `GET`, `POST` or `DELETE`.
    pub method: String,
    /// The URL path, for example `/v1/threads/th_1/prompts`.
    pub path: String,
    /// Decoded query pairs.
    pub query: Vec<(String, String)>,
    /// The authenticated caller (set by the Worker; never taken from a body).
    pub actor: Option<Actor>,
    /// The `Idempotency-Key` header.
    pub idempotency_key: Option<String>,
    /// The JSON body (`Null` when there is none).
    pub body: Value,
    /// `DEV_FIXTURES=1` is set (enables `/v1/dev/...`).
    pub dev_fixtures: bool,
}

/// An answer: status and JSON body (`Null` = no body, for 204).
#[derive(Debug, Clone, PartialEq)]
pub struct ApiResponse {
    pub status: u16,
    pub body: Value,
}

impl From<Result<Reply, ApiError>> for ApiResponse {
    fn from(result: Result<Reply, ApiError>) -> Self {
        match result {
            Ok(reply) => Self {
                status: reply.status,
                body: reply.body,
            },
            Err(error) => Self {
                status: error.status,
                body: error.body(),
            },
        }
    }
}

/// The thread id in a request path, if it is one of ours.
pub fn thread_id_of(path: &str) -> Option<String> {
    let segments: Vec<&str> = path.trim_matches('/').split('/').collect();
    match segments.as_slice() {
        ["v1", "threads", id, ..]
        | ["v1", "dev", "threads", id, ..]
        | ["internal", "threads", id, ..] => Some((*id).to_owned()),
        _ => None,
    }
}

/// Runs one request against the thread. Appended envelopes are in
/// [`ThreadCore::take_appended`] afterwards, also when the answer is an error.
pub fn handle<S: ThreadStore>(
    core: &mut ThreadCore<S>,
    request: &ApiRequest,
    now_ms: u64,
) -> ApiResponse {
    route(core, request, now_ms).into()
}

fn route<S: ThreadStore>(
    core: &mut ThreadCore<S>,
    request: &ApiRequest,
    now_ms: u64,
) -> Result<Reply, ApiError> {
    let path = request.path.trim_matches('/').to_owned();
    let segments: Vec<&str> = path.split('/').collect();
    let method = request.method.as_str();
    // A soft-deleted thread answers 404 on every client route, like an unknown one. The
    // internal routes (the runner's events, the reindex) still reach it.
    if segments.first() == Some(&"v1") {
        core.ensure_live()?;
    }
    match (method, segments.as_slice()) {
        ("GET", ["v1", "threads", _]) => core.thread_info(),
        ("PATCH", ["v1", "threads", _]) => {
            let body: UpdateThreadBody = parse_body(&request.body)?;
            core.update_thread(body, now_ms)
        }
        ("DELETE", ["v1", "threads", _]) => {
            write(core, request, now_ms, |core, _, _| core.delete_thread())
        }
        ("GET", ["v1", "threads", _, "pending"]) => core.list_pending(),
        ("POST", ["v1", "threads", _, "prompts"]) => {
            let body: PromptBody = parse_body(&request.body)?;
            write(core, request, now_ms, |core, actor, key| {
                core.submit_prompt(actor, key, body, now_ms)
            })
        }
        ("DELETE", ["v1", "threads", _, "pending", task_id]) => {
            write(core, request, now_ms, |core, _, _| {
                core.remove_pending(task_id, now_ms)
            })
        }
        ("POST", ["v1", "threads", _, "interrupts", interrupt_id]) => {
            #[derive(Deserialize)]
            struct AnswerBody {
                outcome: ResolvedOutcome,
            }
            let body: AnswerBody = parse_body(&request.body)?;
            write(core, request, now_ms, |core, actor, key| {
                core.answer_interrupt(actor, key, interrupt_id, body.outcome, now_ms)
            })
        }
        ("POST", ["v1", "threads", _, "cancel"]) => {
            #[derive(Deserialize)]
            struct CancelBody {
                task_id: String,
            }
            let body: CancelBody = parse_body(&request.body)?;
            write(core, request, now_ms, |core, actor, key| {
                core.cancel(actor, key, &body.task_id, now_ms)
            })
        }
        ("POST", ["v1", "threads", _, "session"]) => {
            let body: SessionBody = parse_body(&request.body)?;
            write(core, request, now_ms, |core, _, key| {
                core.session_config(key, body, now_ms)
            })
        }
        ("POST", ["v1", "dev", "threads", _, "events"]) => {
            if !request.dev_fixtures {
                return Err(ApiError::not_found(format!(
                    "no route for {method} {}",
                    request.path
                )));
            }
            append_events(core, &request.body, now_ms)
        }
        ("POST", ["internal", "threads", _, "events"]) => {
            append_events(core, &request.body, now_ms)
        }
        ("GET", ["internal", "threads", _, "commands"]) => {
            let limit = request
                .query
                .iter()
                .find(|(key, _)| key == "limit")
                .and_then(|(_, value)| value.parse::<usize>().ok())
                .unwrap_or(DEFAULT_COMMAND_LIMIT);
            Ok(Reply::new(
                200,
                json!({ "commands": core.pending_commands(limit)? }),
            ))
        }
        ("POST", ["internal", "threads", _, "commands", "ack"]) => {
            #[derive(Deserialize)]
            struct AckBody {
                ids: Vec<u64>,
            }
            let body: AckBody = parse_body(&request.body)?;
            let acked = core.ack_commands(&body.ids)?;
            Ok(Reply::new(200, json!({ "acked": acked })))
        }
        ("POST", ["internal", "threads", _, "create"]) => {
            let body: CreateThreadBody = parse_body(&request.body)?;
            write(core, request, now_ms, |core, actor, key| {
                core.create_thread(actor, key, body, now_ms)
            })
        }
        // The cron reconcile (`tasks::reconcile`): only checks the thread exists; the DO then
        // compares and refreshes its D1 index row (`do/thread.rs`).
        ("POST", ["internal", "threads", _, "reindex"]) => {
            core.index_row(0)?;
            Ok(Reply::new(204, Value::Null))
        }
        ("POST", ["internal", "threads", _, "init"]) => {
            let info: Map<String, Value> = parse_body(&request.body)?;
            core.init_thread(&info, now_ms)?;
            core.thread_info()
        }
        _ => Err(ApiError::not_found(format!(
            "no route for {method} {}",
            request.path
        ))),
    }
}

fn parse_body<T: serde::de::DeserializeOwned>(body: &Value) -> Result<T, ApiError> {
    serde_json::from_value(body.clone())
        .map_err(|error| ApiError::invalid_request(format!("invalid body: {error}")))
}

/// Runs a client write under its `Idempotency-Key` (required, client-api.md §3).
fn write<S, F>(
    core: &mut ThreadCore<S>,
    request: &ApiRequest,
    now_ms: u64,
    op: F,
) -> Result<Reply, ApiError>
where
    S: ThreadStore,
    F: FnOnce(&mut ThreadCore<S>, &Actor, &str) -> Result<Reply, ApiError>,
{
    let actor = request
        .actor
        .as_ref()
        .ok_or_else(|| ApiError::unauthorized("missing or invalid access token"))?;
    let key = request.idempotency_key.as_deref().ok_or_else(|| {
        ApiError::new(
            400,
            "idempotency_key_required",
            "this request needs an Idempotency-Key header",
        )
    })?;
    if !valid_idempotency_key(key) {
        return Err(ApiError::invalid_request(
            "Idempotency-Key must be 1-128 characters of [A-Za-z0-9_:.-], not starting with local:",
        ));
    }
    let hash = request_hash(&request.method, &request.path, &request.body);
    core.idempotent(actor, key, &hash, now_ms, |core| op(core, actor, key))
}

/// Appends one envelope (the body) or several (`{"envelopes": [...]}`); answers
/// `{"stored": [...]}` with the stamped envelopes. Stops at the first rejected one.
///
/// `{"event_id": "...", "envelope": {...}}` (the frame's shape, tunnel.md §3.3) appends one
/// envelope at most once per `event_id` in 24 hours: a repeat answers `{"stored": [],
/// "duplicate": true}` without a second append (tunnel.md §4.2).
fn append_events<S: ThreadStore>(
    core: &mut ThreadCore<S>,
    body: &Value,
    now_ms: u64,
) -> Result<Reply, ApiError> {
    if let Some(event_id) = body.get("event_id") {
        let event_id = event_id
            .as_str()
            .filter(|id| is_valid_id(id))
            .ok_or_else(|| {
                ApiError::invalid_request("event_id must be 1-128 characters of [A-Za-z0-9_:.-]")
            })?;
        let envelope = body
            .get("envelope")
            .cloned()
            .ok_or_else(|| ApiError::invalid_request("event_id needs an envelope"))?;
        let envelope: Envelope = serde_json::from_value(envelope)
            .map_err(|error| ApiError::invalid_request(format!("invalid envelope: {error}")))?;
        return Ok(match core.append_once(event_id, envelope, now_ms)? {
            Some(stored) => Reply::new(200, json!({ "stored": [stored] })),
            None => Reply::new(200, json!({ "stored": [], "duplicate": true })),
        });
    }
    let raw: Vec<Value> = match body.get("envelopes") {
        Some(Value::Array(list)) => list.clone(),
        _ => vec![body.clone()],
    };
    let mut stored: Vec<Envelope> = Vec::with_capacity(raw.len());
    for value in raw {
        let envelope: Envelope = serde_json::from_value(value)
            .map_err(|error| ApiError::invalid_request(format!("invalid envelope: {error}")))?;
        stored.push(core.append(envelope, now_ms)?);
    }
    Ok(Reply::new(200, json!({ "stored": stored })))
}
