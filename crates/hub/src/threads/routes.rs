//! The Worker side of the thread routes (wasm32 only): authenticate, then forward to the
//! Thread DO named by the thread id (`THREAD` binding, `id_from_name`).
//!
//! | Route                                                | Auth                      |
//! | ---------------------------------------------------- | ------------------------- |
//! | `GET  /v1/threads/{id}`                              | session token (viewer+), or `Bearer dev` + `DEV_FIXTURES=1` |
//! | `GET  /v1/threads/{id}/subscribe?after=<seq>` (WebSocket) | same                 |
//! | `POST /v1/threads/{id}/prompts`                      | same                      |
//! | `GET  /v1/threads/{id}/pending`                      | same                      |
//! | `DELETE /v1/threads/{id}/pending/{task_id}`          | same                      |
//! | `POST /v1/threads/{id}/interrupts/{interrupt_id}`    | same                      |
//! | `POST /v1/threads/{id}/cancel`                       | same                      |
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
    extract::{Path, Request, State},
    http::{HeaderName, HeaderValue, StatusCode},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use protocol::turn_event::is_valid_id;
use worker::Env;

use super::auth::dev_fixtures_enabled;
use super::error::ApiError;
use crate::auth::routes::authorize_thread_request;
use crate::worker::AppState;

/// The Durable Object binding of the Thread DO (wrangler.toml).
const THREAD_BINDING: &str = "THREAD";

/// Header carrying the authenticated actor (JSON) from the Worker to the DO.
const ACTOR_HEADER: &str = "x-sm-actor";

/// The `/v1/threads/...` routes of M2-008; merged into the Worker router by `worker.rs`.
pub fn thread_routes() -> Router<AppState> {
    Router::new()
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
