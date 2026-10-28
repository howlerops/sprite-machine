//! The Worker routes for machines (wasm32 only, M2B-002):
//!
//! | Route                                            | Auth                                         |
//! | ------------------------------------------------ | -------------------------------------------- |
//! | `POST /v1/machines`                              | session token, admin; audited                |
//! | `GET /v1/machines`                               | session token, viewer                        |
//! | `DELETE /v1/machines/{id}`                       | session token, admin; audited                |
//! | `GET /v1/machines/{id}/tunnel` (WebSocket)       | `Bearer <machine credential>`, checked by the Machine DO |
//! | `POST /v1/dev/machines/{id}/credential`          | `Bearer dev` + `DEV_FIXTURES=1`, no D1       |
//! | `POST /v1/dev/machines/{id}/commands`            | same: push commands as a Thread DO would     |
//! | `GET /v1/dev/machines/{id}/state`                | same: the Machine DO's state                 |
//! | `POST /v1/dev/threads/{id}/init`                 | same: the Thread DO init hook                |
//!
//! The tunnel route forwards the upgrade to the Machine DO named by the machine id, and the DO
//! authenticates the credential itself (tunnel.md §2). Session-token routes use the auth of
//! M2-003a (`authenticate`, `Principal::require`, `audit::record` inside `admin`). The dev
//! routes never touch D1, so the tracer (which runs without migrations) can use them.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{delete, get, post},
};
use protocol::turn_event::is_valid_id;
use serde_json::{Value, json};
use worker::{Env, Method};

use super::admin;
use super::credential::{hash, new_credential};
use super::internal::{DoControl, MACHINE_BINDING, THREAD_BINDING, call};
use crate::auth::routes::error_response;
use crate::auth::service::parse_body;
use crate::auth::{authenticate, bearer_token, dev_fixtures_enabled};
use crate::db::d1::D1;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 64 * 1024;

/// The org of machines issued by the dev credential route.
const DEV_ORG_ID: &str = crate::auth::DEV_ORG_ID;

/// The machine routes; merged into the Worker router by `worker.rs`.
pub fn machine_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/machines", post(create).get(list))
        .route("/v1/machines/{machine_id}", delete(revoke))
        .route("/v1/machines/{machine_id}/tunnel", get(tunnel))
        .route(
            "/v1/dev/machines/{machine_id}/credential",
            post(dev_credential),
        )
        .route("/v1/dev/machines/{machine_id}/commands", post(dev_commands))
        .route("/v1/dev/machines/{machine_id}/state", get(dev_state))
        .route("/v1/dev/threads/{thread_id}/init", post(dev_thread_init))
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

fn valid_machine_id(id: &str) -> Result<&str, ApiError> {
    if is_valid_id(id) {
        Ok(id)
    } else {
        Err(ApiError::invalid_request(
            "machine_id must be 1-128 characters of [A-Za-z0-9_:.-]",
        ))
    }
}

/// The dev routes need `DEV_FIXTURES=1` (else they do not exist) and the literal `Bearer dev`.
fn require_dev(env: &Env, headers: &HeaderMap) -> Result<(), ApiError> {
    if !dev_fixtures(env) {
        return Err(ApiError::not_found("no such route"));
    }
    match bearer_token(authorization(headers).as_deref()) {
        Some("dev") => Ok(()),
        _ => Err(ApiError::unauthorized("missing or invalid access token")),
    }
}

#[worker::send]
async fn create(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (headers, body) = read_body(req).await?;
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        admin::create_machine(
            &db,
            &DoControl { env: &env },
            &caller,
            &body,
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    respond(result, StatusCode::CREATED)
}

#[worker::send]
async fn list(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        admin::list_machines(&db, &caller).await
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
        let machine_id = valid_machine_id(&machine_id)?;
        let headers = req.headers().clone();
        let database = database(&env)?;
        let db = D1::new(&database);
        let caller =
            authenticate(&db, authorization(&headers).as_deref(), dev_fixtures(&env)).await?;
        admin::revoke_machine(
            &db,
            &DoControl { env: &env },
            &caller,
            machine_id,
            &request_id_of(&headers),
        )
        .await
    }
    .await;
    respond(result, StatusCode::OK)
}

/// `GET /v1/machines/{id}/tunnel`: the upgrade goes to the Machine DO unchanged; the DO checks
/// the subprotocol and the credential.
#[worker::send]
async fn tunnel(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let result: Result<Response, ApiError> = async {
        let machine_id = valid_machine_id(&machine_id)?;
        let internal = |error: worker::Error| ApiError::internal(error.to_string());
        let namespace = env.durable_object(MACHINE_BINDING).map_err(internal)?;
        let stub = namespace
            .id_from_name(machine_id)
            .and_then(|id| id.get_stub())
            .map_err(internal)?;
        let req = worker::Request::try_from(req).map_err(internal)?;
        let response = stub.fetch_with_request(req).await.map_err(internal)?;
        Ok(response.into())
    }
    .await;
    result.unwrap_or_else(|error| error_response(&error))
}

/// `POST /v1/dev/machines/{id}/credential`: mints a credential for `machine_id` and gives it to
/// the Machine DO, without D1 (so the tracer can use it). Replaces an earlier credential.
#[worker::send]
async fn dev_credential(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let result = async {
        require_dev(&env, req.headers())?;
        let machine_id = valid_machine_id(&machine_id)?;
        let credential = new_credential()?;
        DoControl { env: &env }
            .provision_replacing(machine_id, DEV_ORG_ID, &hash(&credential))
            .await?;
        Ok(json!({ "machine_id": machine_id, "credential": credential }))
    }
    .await;
    respond(result, StatusCode::CREATED)
}

/// Forwards a JSON body or a GET to a DO route and returns its status and body.
async fn forward_json(
    env: &Env,
    binding: &str,
    name: &str,
    method: Method,
    path: &str,
    body: Option<&Value>,
) -> Response {
    match call(env, binding, name, method, path, body).await {
        Ok((status, text)) => {
            let status = StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY);
            let body = serde_json::from_str::<Value>(&text).unwrap_or(Value::Null);
            (status, Json(body)).into_response()
        }
        Err(error) => error_response(&ApiError::internal(error.to_string())),
    }
}

/// `POST /v1/dev/machines/{id}/commands`: the push a Thread DO makes, for tests.
#[worker::send]
async fn dev_commands(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let checked = async {
        require_dev(&env, req.headers())?;
        valid_machine_id(&machine_id)?;
        read_body(req).await
    }
    .await;
    match checked {
        Err(error) => error_response(&error),
        Ok((_, body)) => {
            let path = format!("/internal/machines/{machine_id}/commands");
            forward_json(
                &env,
                MACHINE_BINDING,
                &machine_id,
                Method::Post,
                &path,
                Some(&body),
            )
            .await
        }
    }
}

/// `GET /v1/dev/machines/{id}/state`.
#[worker::send]
async fn dev_state(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let checked =
        require_dev(&env, req.headers()).and_then(|()| valid_machine_id(&machine_id).map(drop));
    match checked {
        Err(error) => error_response(&error),
        Ok(()) => {
            let path = format!("/internal/machines/{machine_id}/state");
            forward_json(&env, MACHINE_BINDING, &machine_id, Method::Get, &path, None).await
        }
    }
}

/// `POST /v1/dev/threads/{id}/init`: the Thread DO init hook (`machine_id`, `agent`,
/// `workspace`, `permission_mode`, `title`), until thread creation exists.
#[worker::send]
async fn dev_thread_init(
    State(env): State<AppState>,
    Path(params): Path<HashMap<String, String>>,
    req: Request,
) -> Response {
    let checked = async {
        require_dev(&env, req.headers())?;
        let thread_id = params
            .get("thread_id")
            .filter(|id| is_valid_id(id))
            .cloned()
            .ok_or_else(|| ApiError::invalid_request("thread_id must be 1-128 characters"))?;
        let (_, body) = read_body(req).await?;
        Ok::<_, ApiError>((thread_id, body))
    }
    .await;
    match checked {
        Err(error) => error_response(&error),
        Ok((thread_id, body)) => {
            let path = format!("/internal/threads/{thread_id}/init");
            forward_json(
                &env,
                THREAD_BINDING,
                &thread_id,
                Method::Post,
                &path,
                Some(&body),
            )
            .await
        }
    }
}
