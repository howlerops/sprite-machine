//! `POST /v1/dev/workspaces/{workspace_id}/{method}` (wasm32 only, M0-008b): a dev-fixture route
//! that drives one Workspace DO with a raw JSON body, so `scripts/workspace-dev-test.sh` (and a
//! developer with `curl`) can exercise the binding end to end. It answers `404` unless the Worker
//! var `DEV_FIXTURES` is `1`, and the DO's own answer (status and `Result` body) is passed through.

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::post,
};
use worker::Env;

use super::binding::WorkspaceDoBinding;
use super::{ErrorCode, is_rpc_method, valid_workspace_id};
use crate::router::api_error;
use crate::worker::AppState;

/// The fixture route, merged into the hub router (`worker.rs`).
pub fn routes() -> Router<AppState> {
    Router::new().route("/v1/dev/workspaces/{workspace_id}/{method}", post(dev_rpc))
}

/// True only when the `DEV_FIXTURES` var is exactly `1`. A missing var is off.
fn fixtures_enabled(env: &Env) -> bool {
    env.var("DEV_FIXTURES")
        .map(|value| value.to_string() == "1")
        .unwrap_or(false)
}

#[worker::send]
async fn dev_rpc(
    State(env): State<AppState>,
    Path((workspace_id, method)): Path<(String, String)>,
    body: String,
) -> Response {
    if !fixtures_enabled(&env) {
        return error_response(StatusCode::NOT_FOUND, "not_found", "no such route");
    }
    if !valid_workspace_id(&workspace_id) {
        return error_response(
            StatusCode::BAD_REQUEST,
            ErrorCode::InvalidArgument.as_str(),
            "workspace id must be 1 to 128 of [A-Za-z0-9_.-]",
        );
    }
    if !is_rpc_method(&method) {
        return error_response(
            StatusCode::NOT_FOUND,
            "not_found",
            &format!("unknown method {method}"),
        );
    }
    match WorkspaceDoBinding::new(&env, &workspace_id)
        .post_raw(&method, &body)
        .await
    {
        Ok((status, text)) => (
            StatusCode::from_u16(status).unwrap_or(StatusCode::BAD_GATEWAY),
            [(header::CONTENT_TYPE, "application/json")],
            Body::from(text),
        )
            .into_response(),
        Err(error) => error_response(StatusCode::BAD_GATEWAY, error.code.as_str(), &error.message),
    }
}

fn error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (status, Json(api_error(code, message))).into_response()
}
