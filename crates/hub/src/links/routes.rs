//! The link routes (M2-010, wasm32 only): the two association documents from the config vars,
//! and the `/t/{thread_id}` redirect. Pure logic is in `links.rs`.

use axum::{
    Router,
    extract::{Path, State},
    http::{StatusCode, header},
    response::{IntoResponse, Response},
    routing::get,
};
use serde_json::Value;
use worker::Env;

use super::{LinkConfig, apple_app_site_association, asset_links, deep_link_location};
use crate::auth::routes::error_response;
use crate::threads::ApiError;
use crate::worker::AppState;

/// The link routes, merged into the Worker router by `worker.rs`.
pub fn link_routes() -> Router<AppState> {
    Router::new()
        .route(
            "/.well-known/apple-app-site-association",
            get(apple_site_association),
        )
        .route("/.well-known/assetlinks.json", get(android_asset_links))
        .route("/t/{thread_id}", get(deep_link))
}

#[worker::send]
async fn apple_site_association(State(env): State<AppState>) -> Response {
    json_or_not_found(apple_app_site_association(&config(&env)))
}

#[worker::send]
async fn android_asset_links(State(env): State<AppState>) -> Response {
    json_or_not_found(asset_links(&config(&env)))
}

/// `302` to the web page's thread route; `404` for an id that is not a valid thread id.
async fn deep_link(Path(thread_id): Path<String>) -> Response {
    match deep_link_location(&thread_id) {
        Some(location) => (StatusCode::FOUND, [(header::LOCATION, location)]).into_response(),
        None => error_response(&ApiError::not_found(format!("no thread {thread_id}"))),
    }
}

fn config(env: &Env) -> LinkConfig {
    LinkConfig::from_vars(|name| env.var(name).ok().map(|value| value.to_string()))
}

/// The document as `application/json` (no redirect, no extension, as the platforms require),
/// or `404` when the config does not produce one.
fn json_or_not_found(document: Option<Value>) -> Response {
    match document {
        Some(document) => (
            StatusCode::OK,
            [(header::CONTENT_TYPE, "application/json")],
            document.to_string(),
        )
            .into_response(),
        None => error_response(&ApiError::not_found("no app link document is configured")),
    }
}
