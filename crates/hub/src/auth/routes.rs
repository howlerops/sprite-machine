//! The auth routes (wasm32 only) and the thread-route gate:
//!
//! | Route                                  | Needs                                            |
//! | -------------------------------------- | ------------------------------------------------ |
//! | `POST /v1/auth/bootstrap`              | `Bearer <BOOTSTRAP_OWNER_TOKEN>`, once           |
//! | `POST /v1/auth/sessions`               | admin (an owner or admin issues a session)       |
//! | `DELETE /v1/auth/sessions/current`     | any valid session (revokes itself)               |
//! | `DELETE /v1/auth/sessions/{id}`        | admin                                            |
//! | `GET /v1/dev/audit?after=&limit=`      | admin, and 404 unless `DEV_FIXTURES=1`           |

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use serde_json::Value;
use worker::Env;

use super::policy::Policy;
use super::service;
use super::{Principal, authenticate, dev_fixtures_enabled};
use crate::db::Role;
use crate::db::d1::D1;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 64 * 1024;

/// The `/v1/auth/...` routes, merged into the Worker router by `worker.rs`.
pub fn auth_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/bootstrap", post(bootstrap))
        .route("/v1/auth/sessions", post(issue))
        .route("/v1/auth/sessions/current", delete(revoke_current))
        .route("/v1/auth/sessions/{session_id}", delete(revoke_other))
        .route("/v1/dev/audit", get(dev_audit))
}

/// Error as the client-api.md §4 response.
pub fn error_response(error: &ApiError) -> Response {
    (
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(error.body()),
    )
        .into_response()
}

fn dev_fixtures(env: &Env) -> bool {
    dev_fixtures_enabled(
        env.var("DEV_FIXTURES")
            .ok()
            .map(|v| v.to_string())
            .as_deref(),
    )
}

fn authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

/// The request id for the audit row: the caller's valid `x-request-id`, else a fresh one.
fn request_id_of(headers: &HeaderMap) -> String {
    let incoming = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    request_id(incoming, worker::Date::now().as_millis())
}

fn database(env: &Env) -> Result<worker::D1Database, ApiError> {
    env.d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))
}

/// The thread-route gate (see [`service::authorize_thread`]) for one request.
pub async fn authorize_thread_request(
    env: &Env,
    headers: &HeaderMap,
    method: &str,
    path: &str,
) -> Result<(Principal, Policy), ApiError> {
    let database = database(env)?;
    service::authorize_thread(
        &D1::new(&database),
        authorization(headers).as_deref(),
        dev_fixtures(env),
        method,
        path,
        &request_id_of(headers),
    )
    .await
}

async fn read_body(req: Request) -> Result<(HeaderMap, Value), ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
    Ok((parts.headers, service::parse_body(&bytes)?))
}

fn respond(result: Result<Value, ApiError>, ok: StatusCode) -> Response {
    match result {
        Ok(body) => (ok, Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

/// The bootstrap secret: a Worker secret (or, locally, a `--var`).
fn bootstrap_secret(env: &Env) -> Option<String> {
    env.secret("BOOTSTRAP_OWNER_TOKEN")
        .map(|secret| secret.to_string())
        .or_else(|_| env.var("BOOTSTRAP_OWNER_TOKEN").map(|var| var.to_string()))
        .ok()
}

#[worker::send]
async fn bootstrap(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        service::bootstrap(
            &D1::new(&database),
            bootstrap_secret(&env).as_deref(),
            authorization(&headers).as_deref(),
            &body,
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    respond(result, StatusCode::CREATED)
}

#[worker::send]
async fn issue(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::issue_session(&db, &caller, &body, &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::CREATED)
}

#[worker::send]
async fn revoke_current(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::revoke_current(&db, &caller, &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn revoke_other(
    State(env): State<AppState>,
    Path(session_id): Path<String>,
    req: Request,
) -> Response {
    let result = async {
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::revoke_other(&db, &caller, &session_id, &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::OK)
}

/// `GET /v1/dev/audit?after=<seq>&limit=<n>`: the caller's org audit rows. A DEV_FIXTURES-only
/// read for the live tests; a production audit read/export is a later item.
#[worker::send]
async fn dev_audit(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        if !dev_fixtures(&env) {
            return Err(ApiError::not_found("no route for GET /v1/dev/audit"));
        }
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller = authenticate(&db, authorization(&headers).as_deref(), true).await?;
        caller.require(Role::Admin)?;
        let (mut after, mut limit) = (0i64, 200i64);
        for pair in req.uri().query().unwrap_or("").split('&') {
            match pair.split_once('=') {
                Some(("after", value)) => after = value.parse().unwrap_or(0),
                Some(("limit", value)) => limit = value.parse().unwrap_or(200).clamp(1, 1000),
                _ => {}
            }
        }
        let entries = crate::audit::list(&db, &caller.org_id, after, limit).await?;
        Ok(serde_json::json!({
            "entries": entries.iter().map(crate::audit::Entry::to_json).collect::<Vec<_>>(),
        }))
    }
    .await;
    respond(result, StatusCode::OK)
}
