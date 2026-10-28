//! `GET /v1/threads` (client-api.md §5.2, M2-010, wasm32 only): the caller's org's threads from
//! the D1 task index. Viewer role; the org comes from the session, so another org's threads are
//! never in the answer. Routed by `threads::routes` (the path is shared with `POST`).

use axum::{
    Json,
    extract::{Request, State},
    http::{StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
};
use serde_json::{Value, json};

use super::query::parse_list_query;
use super::store::{PAGE_SIZE, list_threads, paginate};
use crate::auth::{authenticate, dev_fixtures_enabled, routes::error_response};
use crate::db::Role;
use crate::db::d1::D1;
use crate::threads::ApiError;
use crate::worker::AppState;

/// The `GET /v1/threads` handler: `{threads: [...], next_cursor}`.
#[worker::send]
pub async fn list_threads_route(State(env): State<AppState>, req: Request) -> Response {
    match list_inner(&env, &req).await {
        Ok(body) => (StatusCode::OK, Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

async fn list_inner(env: &worker::Env, req: &Request) -> Result<Value, ApiError> {
    let dev_fixtures = dev_fixtures_enabled(
        env.var("DEV_FIXTURES")
            .ok()
            .map(|value| value.to_string())
            .as_deref(),
    );
    let authorization = req
        .headers()
        .get(AUTHORIZATION)
        .and_then(|value| value.to_str().ok());
    let database = env
        .d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))?;
    let db = D1::new(&database);
    let principal = authenticate(&db, authorization, dev_fixtures).await?;
    principal.require(Role::Viewer)?;

    let query = parse_list_query(req.uri().query())?;
    let rows = list_threads(&db, &principal.org_id, &query, PAGE_SIZE + 1)
        .await
        .map_err(|error| ApiError::internal(format!("tasks: {error}")))?;
    let (page, next) = paginate(rows, PAGE_SIZE);
    Ok(json!({
        "threads": page.iter().map(|row| row.to_json()).collect::<Vec<_>>(),
        "next_cursor": next.map(|cursor| cursor.encode()),
    }))
}
