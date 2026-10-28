//! The fixture routes themselves (wasm32 only); see the parent module.

use axum::{
    Json, Router,
    extract::{Path, Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::post,
};
use protocol::turn_event::is_valid_id;
use serde_json::{Value, json};

use crate::auth::routes::error_response;
use crate::auth::service::parse_body;
use crate::auth::token::new_id;
use crate::auth::{authenticate, dev_fixtures_enabled};
use crate::db::d1::D1;
use crate::db::{Db, Role};
use crate::devices::signin::iso;
use crate::tasks::index::IndexRow;
use crate::tasks::store;
use crate::threads::ApiError;
use crate::worker::AppState;

use super::{gate, presence_state, stored_status};

const MAX_BODY_BYTES: usize = 16 * 1024;

/// The fixture routes; merged into the Worker router by `worker.rs`.
pub fn routes() -> Router<AppState> {
    Router::new()
        .route(
            "/v1/dev/fixtures/machines/{machine_id}/presence",
            post(set_presence),
        )
        .route("/v1/dev/fixtures/threads", post(upsert_thread))
}

fn authorization(headers: &HeaderMap) -> Option<String> {
    headers
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn now_ms() -> i64 {
    i64::try_from(worker::Date::now().as_millis()).unwrap_or(i64::MAX)
}

fn db_failure(error: crate::db::DbError) -> ApiError {
    ApiError::internal(format!("dev fixtures: {error}"))
}

fn database(env: &AppState) -> Result<worker::D1Database, ApiError> {
    env.d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))
}

/// The gate, the body and an admin caller's org id.
async fn admit(env: &AppState, req: Request) -> Result<(String, Value), ApiError> {
    let enabled = dev_fixtures_enabled(
        env.var("DEV_FIXTURES")
            .ok()
            .map(|value| value.to_string())
            .as_deref(),
    );
    gate(enabled)?;
    let (parts, body) = req.into_parts();
    let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
        .await
        .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
    let body = parse_body(&bytes)?;
    let database = database(env)?;
    let caller = authenticate(
        &D1::new(&database),
        authorization(&parts.headers).as_deref(),
        enabled,
    )
    .await?;
    caller.require(Role::Admin)?;
    Ok((caller.org_id, body))
}

fn respond(result: Result<Value, ApiError>) -> Response {
    match result {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

#[worker::send]
async fn set_presence(
    State(env): State<AppState>,
    Path(machine_id): Path<String>,
    req: Request,
) -> Response {
    let result = async {
        let (org_id, body) = admit(&env, req).await?;
        if !is_valid_id(&machine_id) {
            return Err(ApiError::invalid_request("machine_id is not valid"));
        }
        let state = body
            .get("state")
            .and_then(Value::as_str)
            .and_then(presence_state)
            .ok_or_else(|| ApiError::invalid_request("state must be online, asleep or offline"))?;
        let since = iso(now_ms());
        let database = database(&env)?;
        let changed = D1::new(&database)
            .execute(
                "UPDATE machines SET presence = ?, presence_since = ? WHERE id = ? AND org_id = ?",
                &[
                    state.into(),
                    since.as_str().into(),
                    machine_id.as_str().into(),
                    org_id.as_str().into(),
                ],
            )
            .await
            .map_err(db_failure)?;
        if changed == 0 {
            return Err(ApiError::not_found("no such machine"));
        }
        Ok(json!({ "machine_id": machine_id, "state": state, "since": since }))
    }
    .await;
    respond(result)
}

#[worker::send]
async fn upsert_thread(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (org_id, body) = admit(&env, req).await?;
        let text = |key: &str| body.get(key).and_then(Value::as_str);
        let machine_id = text("machine_id")
            .filter(|id| is_valid_id(id))
            .ok_or_else(|| ApiError::invalid_request("machine_id is required"))?;
        let title = text("title").ok_or_else(|| ApiError::invalid_request("title is required"))?;
        let status = match text("status") {
            None => "open",
            Some(name) => stored_status(name)
                .ok_or_else(|| ApiError::invalid_request("status is not a thread status"))?,
        };
        let thread_id = match text("thread_id") {
            Some(id) if is_valid_id(id) => id.to_owned(),
            Some(_) => return Err(ApiError::invalid_request("thread_id is not valid")),
            None => new_id("th")?,
        };
        let updated_at = iso(now_ms());
        let row = IndexRow {
            thread_id: thread_id.clone(),
            machine_id: machine_id.to_owned(),
            agent: "claude".to_owned(),
            workspace_path: String::new(),
            title: title.to_owned(),
            title_source: "user",
            status,
            archived: false,
            deleted: false,
            updated_at: updated_at.clone(),
            last_seq: 0,
        };
        let database = database(&env)?;
        store::upsert(&D1::new(&database), &org_id, &row)
            .await
            .map_err(db_failure)?;
        Ok(json!({ "thread_id": thread_id, "updated_at": updated_at }))
    }
    .await;
    respond(result)
}
