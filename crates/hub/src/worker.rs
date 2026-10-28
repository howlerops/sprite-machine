//! Workers glue for the hub (wasm32 only, M2-001): the axum `Router`, the `fetch` and `queue`
//! entries, the request-id and access-log middleware and the JSON 404.
//!
//! Patterns from `spikes/cf` (docs/spikes/cloudflare.md): `Env` is `!Send`, so it sits in a
//! `SendWrapper` as router state and every handler that awaits a JS future is marked
//! `#[worker::send]`. The router is rebuilt per request, which is cheap.
//!
//! Routes:
//!
//! - `GET /healthz`: build info (`{"name","version"}`).
//! - `/v1/threads/...` and `/v1/dev/threads/...`: the Thread DO routes (M2-008, `threads/routes.rs`).
//! - `/v1/machines/...` and `/v1/dev/machines/...`: the machine admin API and the runner tunnel to
//!   the Machine DO (M2B-002, `machines/routes.rs`).
//! - `/v1/attachments...`: the attachment create (session token) and the signed upload and content
//!   URLs (M2-009, `attachments/routes.rs`).
//! - `/v1/push/...`: Web Push subscriptions, prefs and the VAPID public key (M2-011,
//!   `notify/routes.rs`).
//! - anything else: `404` with the client-api.md §4 error body.
//!
//! Every response carries `x-request-id` and one JSON access-log line goes to the Workers console.

use axum::{
    Json, Router,
    body::Body,
    extract::Request,
    http::{HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::get,
};
use tower_service::Service;
use worker::{
    Context, Env, HttpRequest, MessageBatch, ScheduleContext, ScheduledEvent, event,
    send::SendWrapper,
};

use crate::router::{self, REQUEST_ID_HEADER};

/// Router state: the Worker bindings.
pub type AppState = SendWrapper<Env>;

/// Builds the request router. The middleware wraps every route and the fallback, so even a 404 gets a request id and an access-log line.
pub fn router(env: Env) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(crate::auth::routes::auth_routes())
        .merge(crate::auth::github_routes::github_routes())
        .merge(crate::members::routes::member_routes())
        .merge(crate::threads::routes::thread_routes())
        .merge(crate::machines::routes::machine_routes())
        .merge(crate::devices::routes::device_routes())
        .merge(crate::devices::routes::signin_routes())
        .merge(crate::attachments::routes::attachment_routes())
        .merge(crate::notify::routes::push_routes())
        .merge(crate::links::routes::link_routes())
        .merge(crate::workspace_do::dev::routes())
        .merge(crate::dev_fixtures::routes::routes())
        .fallback(not_found)
        .layer(middleware::from_fn(observe))
        .with_state(SendWrapper::new(env))
}

/// The HTTP entry point: every request goes through [`router`].
#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<Response<Body>> {
    Ok(router(env).call(req).await?)
}

/// The Queue consumer for `sm-notify`: Web Push delivery (M2-011, `queues/notify.rs`).
#[event(queue)]
async fn queue(
    batch: MessageBatch<serde_json::Value>,
    env: Env,
    _ctx: Context,
) -> worker::Result<()> {
    crate::queues_notify::consume(batch, &env).await
}

/// The Cron Trigger (wrangler.toml `[triggers]`): purges expired device pairings and old
/// attempt counters, expired GitHub sign-in requests, codes and invites (M2-003e), and reconciles the task index (M2-010a).
#[event(scheduled)]
async fn scheduled(_event: ScheduledEvent, env: Env, _ctx: ScheduleContext) {
    crate::devices::routes::purge_scheduled(&env).await;
    crate::auth::github_routes::purge_scheduled(&env).await;
    purge_subscribe_tickets(&env).await;
    crate::tasks::reconcile::run(&env).await;
}

/// The cron trigger's ticket purge (M2-008j): expired subscribe tickets and spent ones past their
/// grace (`auth/ticket.rs`).
async fn purge_subscribe_tickets(env: &Env) {
    let Ok(database) = env.d1("DB") else {
        worker::console_log!("subscribe ticket purge failed: no D1 binding DB");
        return;
    };
    let now_ms = i64::try_from(worker::Date::now().as_millis()).unwrap_or(i64::MAX);
    match crate::auth::ticket::purge(&crate::db::d1::D1::new(&database), now_ms).await {
        Ok(deleted) => worker::console_log!("subscribe ticket purge: {deleted} tickets"),
        Err(error) => worker::console_log!("subscribe ticket purge failed: {}", error.message),
    }
}

async fn healthz() -> Json<serde_json::Value> {
    Json(router::health_body())
}

async fn not_found(method: Method, uri: Uri) -> Response {
    let message = format!("no route for {method} {}", uri.path());
    let body = router::api_error("not_found", &message);
    (StatusCode::NOT_FOUND, Json(body)).into_response()
}

/// Assigns the request id, times the request and writes the access-log line.
async fn observe(req: Request, next: Next) -> Response {
    let started = worker::Date::now().as_millis();
    let method = req.method().to_string();
    // The whole path-and-query goes to `router::access_log`, which logs the path only.
    let path_and_query = req.uri().path_and_query().map_or_else(
        || req.uri().path().to_owned(),
        |value| value.as_str().to_owned(),
    );
    let incoming = req
        .headers()
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned);
    let id = router::request_id(incoming.as_deref(), started);

    let mut response = next.run(req).await;
    if let Ok(value) = HeaderValue::from_str(&id) {
        response.headers_mut().insert(REQUEST_ID_HEADER, value);
    }
    let status = response.status().as_u16();
    let elapsed = worker::Date::now().as_millis().saturating_sub(started);
    worker::console_log!(
        "{}",
        router::access_log(&id, &method, &path_and_query, status, elapsed)
    );
    response
}
