//! The Worker routes for device pairing (wasm32 only, M2B-005):
//!
//! | Route                              | Auth                                              |
//! | ---------------------------------- | ------------------------------------------------- |
//! | `POST /v1/devices/code`            | none (rate limit TODO, see `service::start`)      |
//! | `POST /v1/devices/token`           | none; the device code is the secret (RFC 8628)    |
//! | `POST /v1/devices/approve`         | session token, member; audited                    |
//! | `POST /v1/devices/deny`            | session token, member; audited                    |
//! | `GET /v1/devices`                  | session token, viewer                             |
//! | `DELETE /v1/devices/{machine_id}`  | session token, admin; audited                     |
//! | `PUT /v1/devices/policy`           | session token, admin; audited                     |
//!
//! The device code and the credential travel only in JSON bodies, never in a URL.

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use protocol::turn_event::is_valid_id;
use serde_json::Value;
use worker::Env;

use super::service::{self, PollError};
use crate::auth::routes::error_response;
use crate::auth::service::parse_body;
use crate::auth::{authenticate, dev_fixtures_enabled};
use crate::db::d1::D1;
use crate::machines::internal::DoControl;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 16 * 1024;

/// The device routes; merged into the Worker router by `worker.rs`.
pub fn device_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/devices", get(list))
        .route("/v1/devices/code", post(code))
        .route("/v1/devices/token", post(token))
        .route("/v1/devices/approve", post(approve))
        .route("/v1/devices/deny", post(deny))
        .route("/v1/devices/policy", axum::routing::put(policy))
        .route("/v1/devices/{machine_id}", delete(revoke))
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

fn request_id_of(headers: &HeaderMap) -> String {
    let incoming = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    request_id(incoming, worker::Date::now().as_millis())
}

fn now_ms() -> i64 {
    i64::try_from(worker::Date::now().as_millis()).unwrap_or(i64::MAX)
}

fn database(env: &Env) -> Result<worker::D1Database, ApiError> {
    env.d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))
}

async fn read_body(req: Request) -> Result<(HeaderMap, Value), ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
    Ok((parts.headers, parse_body(&bytes)?))
}

fn respond(result: Result<Value, ApiError>, ok: StatusCode) -> Response {
    match result {
        Ok(body) => (ok, Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

/// The hub's public origin, from the request's own `Host`: where the person opens the
/// verification page. Plain `http` only for loopback (local dev).
fn base_url(headers: &HeaderMap) -> String {
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let local =
        host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]");
    format!("{}://{host}", if local { "http" } else { "https" })
}

#[worker::send]
async fn code(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let headers = req.headers().clone();
        let database = database(&env)?;
        service::start(&D1::new(&database), &base_url(&headers), now_ms()).await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn token(State(env): State<AppState>, req: Request) -> Response {
    let result: Result<Value, PollError> = async {
        let (_, body) = read_body(req).await?;
        let device_code = body
            .get("device_code")
            .and_then(Value::as_str)
            .ok_or_else(|| ApiError::invalid_request("device_code is required"))?;
        let database = database(&env)?;
        service::poll(
            &D1::new(&database),
            &DoControl { env: &env },
            device_code,
            now_ms(),
        )
        .await
    }
    .await;
    match result {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(error) => {
            let (status, body) = error.response();
            (
                StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_REQUEST),
                Json(body),
            )
                .into_response()
        }
    }
}

#[worker::send]
async fn approve(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::approve(&db, &caller, &body, now_ms(), &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn deny(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::deny(&db, &caller, &body, now_ms(), &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn list(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::list(&db, &caller).await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn revoke(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let result = async {
        if !is_valid_id(&machine_id) {
            return Err(ApiError::invalid_request(
                "machine_id must be 1-128 characters of [A-Za-z0-9_:.-]",
            ));
        }
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::revoke(
            &db,
            &DoControl { env: &env },
            &caller,
            &machine_id,
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    respond(result, StatusCode::OK)
}

#[worker::send]
async fn policy(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        service::set_policy(&db, &caller, &body, &request_id_of(&headers)).await
    }
    .await;
    respond(result, StatusCode::OK)
}
