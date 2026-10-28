//! The members and invites routes (wasm32 only); the logic and the role rules are in `mod.rs`.

use axum::{
    Json, Router,
    extract::{Request, State},
    http::{HeaderMap, StatusCode, header::AUTHORIZATION},
    response::{IntoResponse, Response},
    routing::{get, patch},
};
use serde_json::Value;

use crate::auth::dev_fixtures_enabled;
use crate::auth::routes::error_response;
use crate::auth::service::parse_body;
use crate::db::d1::D1;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 16 * 1024;

/// The members and invites routes; merged into the Worker router by `worker.rs`.
pub fn member_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/members", get(run))
        .route("/v1/members/{user_id}", patch(run).delete(run))
        .route("/v1/invites", get(run).post(run))
        .route("/v1/invites/{invite_id}", axum::routing::delete(run))
}

/// One handler for all of them: [`super::gate`] picks the policy from the method and path.
#[worker::send]
async fn run(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let method = req.method().to_string();
        let path = req.uri().path().to_owned();
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
            .await
            .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
        let body: Value = parse_body(&bytes)?;
        let database = env
            .d1("DB")
            .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))?;
        let db = D1::new(&database);
        let dev = dev_fixtures_enabled(
            env.var("DEV_FIXTURES")
                .ok()
                .map(|v| v.to_string())
                .as_deref(),
        );
        let authorization = parts
            .headers
            .get(AUTHORIZATION)
            .and_then(|value| value.to_str().ok());
        let (caller, policy) = super::gate(&db, authorization, dev, &method, &path).await?;
        let now = i64::try_from(worker::Date::now().as_millis()).unwrap_or(i64::MAX);
        super::handle(
            &db,
            &caller,
            &policy,
            &method,
            &body,
            now,
            &request_id_of(&parts.headers),
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

fn request_id_of(headers: &HeaderMap) -> String {
    let incoming = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    request_id(incoming, worker::Date::now().as_millis())
}
