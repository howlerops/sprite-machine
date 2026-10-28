//! The Worker side of the thread routes (wasm32 only): authenticate, then forward to the
//! Thread DO named by the thread id (`THREAD` binding, `id_from_name`).
//!
//! | Route                                                | Auth                      |
//! | ---------------------------------------------------- | ------------------------- |
//! | `POST /v1/threads`                                   | session token (member), `Idempotency-Key`; creates the thread (M2-008c) |
//! | `GET  /v1/threads/{id}`                              | session token (viewer+), or `Bearer dev` + `DEV_FIXTURES=1` |
//! | `GET  /v1/threads/{id}/subscribe?after=<seq>` (WebSocket) | same                 |
//! | `POST /v1/threads/{id}/prompts`                      | same                      |
//! | `GET  /v1/threads/{id}/pending`                      | same                      |
//! | `DELETE /v1/threads/{id}/pending/{task_id}`          | same                      |
//! | `POST /v1/threads/{id}/interrupts/{interrupt_id}`    | same                      |
//! | `POST /v1/threads/{id}/cancel`                       | same                      |
//! | `POST /v1/threads/{id}/session`                      | member, `Idempotency-Key`; §5.5a (M2-008b), see `session_config` |
//! | `POST /v1/dev/threads/{id}/events`                   | same, and 404 unless `DEV_FIXTURES=1` |
//!
//! Roles and audit (M2-003a, `auth/policy.rs`): reads need viewer; prompts, approvals and cancel
//! need member; deleting a pending task needs admin. Every mutation writes an audit row first.
//!
//! The authenticated actor travels to the DO in the `x-sm-actor` header; a client-supplied one
//! is overwritten. The DO's `/internal/...` routes are not registered here, so only another
//! Durable Object or Worker code holding a stub can reach them.

use std::collections::HashMap;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use protocol::turn_event::is_valid_id;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use worker::Env;

use super::auth::dev_fixtures_enabled;
use super::error::ApiError;
use super::valid_idempotency_key;
use crate::audit::{self, Event};
use crate::auth::{
    Principal, authenticate, policy::thread_policy, routes::authorize_thread_request,
};
use crate::db::{Db, d1::D1};
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::worker::AppState;

/// The Durable Object binding of the Thread DO (wrangler.toml).
const THREAD_BINDING: &str = "THREAD";

/// Header carrying the authenticated actor (JSON) from the Worker to the DO.
const ACTOR_HEADER: &str = "x-sm-actor";

/// The `/v1/threads/...` routes of M2-008; merged into the Worker router by `worker.rs`.
pub fn thread_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/threads", post(create_thread))
        .route("/v1/threads/{thread_id}", get(forward))
        .route("/v1/threads/{thread_id}/subscribe", get(forward))
        .route("/v1/threads/{thread_id}/prompts", post(forward))
        .route("/v1/threads/{thread_id}/pending", get(forward))
        .route("/v1/threads/{thread_id}/pending/{task_id}", delete(forward))
        .route(
            "/v1/threads/{thread_id}/interrupts/{interrupt_id}",
            post(forward),
        )
        .route("/v1/threads/{thread_id}/cancel", post(forward))
        .route("/v1/threads/{thread_id}/session", post(forward))
        .route("/v1/dev/threads/{thread_id}/events", post(forward))
}

#[worker::send]
async fn forward(
    State(env): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    req: Request,
) -> Response {
    match forward_inner(&env, &params, req).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(error.body()),
        )
            .into_response(),
    }
}

async fn forward_inner(
    env: &Env,
    params: &HashMap<String, String>,
    mut req: Request,
) -> Result<Response, ApiError> {
    let dev_fixtures = dev_fixtures_enabled(
        env.var("DEV_FIXTURES")
            .ok()
            .map(|v| v.to_string())
            .as_deref(),
    );
    if req.uri().path().starts_with("/v1/dev/") && !dev_fixtures {
        return Err(ApiError::not_found(format!(
            "no route for {} {}",
            req.method(),
            req.uri().path()
        )));
    }
    let (principal, _policy) =
        authorize_thread_request(env, req.headers(), req.method().as_str(), req.uri().path())
            .await?;
    let actor = principal.actor();
    let thread_id = params
        .get("thread_id")
        .filter(|id| is_valid_id(id))
        .ok_or_else(|| {
            ApiError::invalid_request("thread_id must be 1-128 characters of [A-Za-z0-9_:.-]")
        })?;

    let actor_json =
        serde_json::to_string(&actor).map_err(|e| ApiError::internal(e.to_string()))?;
    let value =
        HeaderValue::from_str(&actor_json).map_err(|e| ApiError::internal(e.to_string()))?;
    req.headers_mut()
        .insert(HeaderName::from_static(ACTOR_HEADER), value);

    to_thread(env, thread_id, req).await
}

/// Hands a request to the Thread DO named `thread_id` and returns its answer.
async fn to_thread(env: &Env, thread_id: &str, req: Request) -> Result<Response, ApiError> {
    let internal = |error: worker::Error| ApiError::internal(error.to_string());
    let namespace = env.durable_object(THREAD_BINDING).map_err(internal)?;
    let stub = namespace
        .id_from_name(thread_id)
        .and_then(|id| id.get_stub())
        .map_err(internal)?;
    let req = worker::Request::try_from(req).map_err(internal)?;
    let response = stub.fetch_with_request(req).await.map_err(internal)?;
    Ok(response.into())
}

/// The body limit of `POST /v1/threads` (a prompt's parts travel in it).
const MAX_CREATE_BYTES: usize = 64 * 1024;

/// `POST /v1/threads` (client-api.md §5.2, M2-008c): member role, audited as `thread.create`.
///
/// The thread id is derived from the caller and the `Idempotency-Key`, so a replay of the same
/// request reaches the same Thread DO and gets its stored answer (no second thread). The Thread
/// DO creates the thread, records the host machine (init hook), and with a prompt queues the
/// `dispatch` command for the Machine DO.
///
/// The role and audit action come from `auth::policy` (`thread_policy("POST", "/v1/threads")`).
/// `authorize_thread` cannot run the gate because the route has no thread id in its path and the
/// audit row names the derived one, so the route authenticates, checks the policy's role, then
/// checks the machine (M2-008d), and only then audits and forwards. A machine that does not
/// exist, is revoked, or belongs to another org is one `404 not_found` and writes nothing.
#[worker::send]
async fn create_thread(State(env): State<AppState>, req: Request) -> Response {
    match create_inner(&env, req).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
            Json(error.body()),
        )
            .into_response(),
    }
}

async fn create_inner(env: &Env, req: Request) -> Result<Response, ApiError> {
    let (parts, body) = req.into_parts();
    let headers: &HeaderMap = &parts.headers;
    let key = headers
        .get("Idempotency-Key")
        .and_then(|value| value.to_str().ok())
        .ok_or_else(|| {
            ApiError::new(
                400,
                "idempotency_key_required",
                "this request needs an Idempotency-Key header",
            )
        })?;
    if !valid_idempotency_key(key) {
        return Err(ApiError::invalid_request(
            "Idempotency-Key must be 1-128 characters of [A-Za-z0-9_:.-]",
        ));
    }
    let bytes = axum::body::to_bytes(body, MAX_CREATE_BYTES)
        .await
        .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
    let value: Value = serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::invalid_request(format!("body is not JSON: {error}")))?;

    let database = env
        .d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))?;
    let db = D1::new(&database);
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let principal: Principal = authenticate(&db, authorization, dev_fixtures(env)).await?;
    let policy = thread_policy("POST", "/v1/threads")
        .ok_or_else(|| ApiError::internal("no policy for POST /v1/threads"))?;
    principal.require(policy.role)?;

    let thread_id = derived_thread_id(&principal.user_id, key);
    let request_id_header = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    let request_id = request_id(request_id_header, worker::Date::now().as_millis());
    let machine_id = value.get("machine_id").and_then(Value::as_str);
    let machine_id = machine_id
        .filter(|id| is_valid_id(id))
        .ok_or_else(|| ApiError::invalid_request("machine_id is required"))?;
    require_machine_of_org(&db, machine_id, &principal.org_id).await?;
    audit::record(
        &db,
        &Event::by(
            &principal,
            policy.action.unwrap_or("thread.create"),
            "thread",
            &thread_id,
            &request_id,
            json!({ "machine_id": machine_id }),
        ),
    )
    .await?;

    let actor_json = serde_json::to_string(&principal.actor())
        .map_err(|error| ApiError::internal(error.to_string()))?;
    let forwarded = Request::builder()
        .method("POST")
        // Absolute, as the Worker's request conversion needs; the DO routes on the path only.
        .uri(format!(
            "https://do.internal/internal/threads/{thread_id}/create"
        ))
        .header(ACTOR_HEADER, actor_json)
        .header("Idempotency-Key", key)
        .header("content-type", "application/json")
        .body(Body::from(bytes))
        .map_err(|error| ApiError::internal(error.to_string()))?;
    to_thread(env, &thread_id, forwarded).await
}

/// `404 not_found` unless `machine_id` is a machine of `org_id` that is not revoked. The same
/// answer for an unknown machine, a revoked one and another org's, so nothing leaks across orgs.
async fn require_machine_of_org(
    db: &impl Db,
    machine_id: &str,
    org_id: &str,
) -> Result<(), ApiError> {
    let rows = db
        .query(
            "SELECT id FROM machines WHERE id = ? AND org_id = ? AND revoked_at IS NULL",
            &[machine_id.into(), org_id.into()],
        )
        .await
        .map_err(|error| ApiError::internal(format!("threads: {error}")))?;
    if rows.is_empty() {
        return Err(ApiError::not_found(format!("no machine {machine_id}")));
    }
    Ok(())
}

/// `th_` plus 32 hex digits of `SHA-256(user, key)`: stable for one user and key, distinct
/// across users. Not a secret; only the Idempotency-Key scope needs it to be unique.
fn derived_thread_id(user_id: &str, key: &str) -> String {
    let digest = Sha256::digest(format!("{user_id}\n{key}").as_bytes());
    let hex: String = digest
        .iter()
        .take(16)
        .map(|byte| format!("{byte:02x}"))
        .collect();
    format!("th_{hex}")
}

fn dev_fixtures(env: &Env) -> bool {
    dev_fixtures_enabled(
        env.var("DEV_FIXTURES")
            .ok()
            .map(|v| v.to_string())
            .as_deref(),
    )
}
