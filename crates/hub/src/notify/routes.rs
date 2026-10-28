//! The push routes (wasm32 only), merged into the Worker router by `worker.rs`.
//!
//! | Route                           | Needs                                                      |
//! | ------------------------------- | ---------------------------------------------------------- |
//! | `POST /v1/push/subscriptions`   | member; `{endpoint, keys:{p256dh, auth}}`; 201 new, 200 known endpoint |
//! | `DELETE /v1/push/subscriptions` | member; `{endpoint}`; 204                                  |
//! | `GET /v1/push/prefs`            | member                                                     |
//! | `PUT /v1/push/prefs`            | member; replaces the prefs                                 |
//! | `GET /v1/push/vapid-public-key` | any valid session; the `applicationServerKey` to subscribe with |

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use serde_json::{Value, json};
use worker::Env;

use super::env::{dev_fixtures, load_vapid};
use super::service;
use crate::auth::{Principal, authenticate};
use crate::db::d1::D1;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 16 * 1024;

pub fn push_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/push/subscriptions",
            post(subscribe).delete(unsubscribe),
        )
        .route("/v1/push/prefs", get(get_prefs).put(put_prefs))
        .route("/v1/push/vapid-public-key", get(vapid_public_key))
}

fn error_response(error: &ApiError) -> Response {
    (
        StatusCode::from_u16(error.status).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR),
        Json(error.body()),
    )
        .into_response()
}

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

async fn caller(env: &Env, db: &D1<'_>, headers: &HeaderMap) -> Result<Principal, ApiError> {
    let authorization = headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    authenticate(db, authorization, dev_fixtures(env)).await
}

/// The JSON body of a request; an empty body is `null`.
async fn read_body(req: Request) -> Result<(HeaderMap, Value), ApiError> {
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
    if bytes.iter().all(u8::is_ascii_whitespace) {
        return Ok((parts.headers, Value::Null));
    }
    let value = serde_json::from_slice(&bytes)
        .map_err(|error| ApiError::invalid_request(format!("body is not JSON: {error}")))?;
    Ok((parts.headers, value))
}

#[worker::send]
async fn subscribe(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let principal = caller(&env, &db, &headers).await?;
        service::subscribe(
            &db,
            &principal,
            &body,
            dev_fixtures(&env),
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    match result {
        Ok((status, body)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::OK),
            Json(body),
        )
            .into_response(),
        Err(error) => error_response(&error),
    }
}

#[worker::send]
async fn unsubscribe(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let principal = caller(&env, &db, &headers).await?;
        service::unsubscribe(&db, &principal, &body, &request_id_of(&headers)).await
    }
    .await;
    match result {
        Ok(()) => StatusCode::NO_CONTENT.into_response(),
        Err(error) => error_response(&error),
    }
}

#[worker::send]
async fn get_prefs(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let database = database(&env)?;
        let db = D1::new(&database);
        let principal = caller(&env, &db, req.headers()).await?;
        service::get_prefs(&db, &principal).await
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(&error),
    }
}

#[worker::send]
async fn put_prefs(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let principal = caller(&env, &db, &headers).await?;
        service::put_prefs(
            &db,
            &principal,
            &body,
            worker::Date::now().as_millis(),
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(&error),
    }
}

/// The key the phone passes to `pushManager.subscribe({applicationServerKey})`: derived from
/// the VAPID secret, else the `VAPID_PUBLIC_KEY` var, else `404`.
#[worker::send]
async fn vapid_public_key(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let database = database(&env)?;
        let db = D1::new(&database);
        caller(&env, &db, req.headers()).await?;
        let key = load_vapid(&env)
            .map(|vapid| vapid.public_key().to_owned())
            .or_else(|| {
                env.var("VAPID_PUBLIC_KEY")
                    .ok()
                    .map(|var| var.to_string())
                    .filter(|key| !key.is_empty())
            })
            .ok_or_else(|| ApiError::not_found("web push is not configured on this hub"))?;
        Ok::<_, ApiError>(json!({ "public_key": key }))
    }
    .await;
    match result {
        Ok(body) => Json(body).into_response(),
        Err(error) => error_response(&error),
    }
}
