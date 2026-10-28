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
//! - tracer routes (MVP-000, NO AUTH, see `tracer/worker.rs`), forwarded to the `TracerHub`
//!   Durable Object. `GET /health` is the tracer's liveness probe (`cli e2e tracer` polls it).
//! - anything else: `404` with the client-api.md §4 error body.
//!
//! Every response carries `x-request-id` and one JSON access-log line goes to the Workers console.

use axum::{
    Json, Router,
    body::Body,
    extract::{Request, State},
    http::{HeaderValue, Method, StatusCode, Uri},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
};
use tower_service::Service;
use worker::{Context, Env, HttpRequest, MessageBatch, event, send::SendWrapper};

use crate::router::{self, REQUEST_ID_HEADER};

/// Router state: the Worker bindings.
pub type AppState = SendWrapper<Env>;

/// Durable Object binding and the single instance name used by the tracer.
const HUB_BINDING: &str = "HUB";
const HUB_NAME: &str = "tracer";

/// The Queue bound as `NOTIFY` (producer and consumer, see wrangler.toml).
const NOTIFY_QUEUE: &str = "sm-notify";

/// Builds the request router. Tracer routes are merged in; the middleware wraps every route
/// and the fallback, so even a 404 gets a request id and an access-log line.
pub fn router(env: Env) -> Router {
    Router::new()
        .route("/healthz", get(healthz))
        .merge(tracer_routes())
        .fallback(not_found)
        .layer(middleware::from_fn(observe))
        .with_state(SendWrapper::new(env))
}

/// The HTTP entry point: every request goes through [`router`].
#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<Response<Body>> {
    Ok(router(env).call(req).await?)
}

/// The Queue consumer for `sm-notify`. Nothing consumes yet, so the batch is acknowledged
/// (no retries) and logged.
#[event(queue)]
async fn queue(
    batch: MessageBatch<serde_json::Value>,
    _env: Env,
    _ctx: Context,
) -> worker::Result<()> {
    let messages = batch.messages()?.len();
    worker::console_log!("{}", router::queue_log(NOTIFY_QUEUE, messages));
    batch.ack_all();
    Ok(())
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
    let path = req.uri().path().to_owned();
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
        router::access_log(&id, &method, &path, status, elapsed)
    );
    response
}

// ---- tracer routes (MVP-000 stub; replaced by M2-008 + M2B-002) ----------------------------

/// The tracer's routes. Each one forwards the request, WebSocket upgrades included, to the
/// `TracerHub` Durable Object (`tracer/worker.rs`), which does the work.
fn tracer_routes() -> Router<AppState> {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/v1/machines/{machine_id}/tunnel", get(to_hub))
        .route("/threads/{thread_id}/subscribe", get(to_hub))
        .route("/threads/{thread_id}/prompts", post(to_hub))
        .route(
            "/threads/{thread_id}/interrupts/{interrupt_id}",
            post(to_hub),
        )
}

/// Forwards the request unchanged (WebSocket upgrades included) to the tracer DO.
#[worker::send]
async fn to_hub(State(env): State<AppState>, req: Request) -> Result<Response, AppError> {
    let stub = env
        .durable_object(HUB_BINDING)?
        .id_from_name(HUB_NAME)?
        .get_stub()?;
    let req = worker::Request::try_from(req)?;
    let res = stub.fetch_with_request(req).await?;
    Ok(res.into())
}

/// A worker error becomes a 500 with the message as body.
struct AppError(worker::Error);

impl From<worker::Error> for AppError {
    fn from(e: worker::Error) -> Self {
        Self(e)
    }
}

impl IntoResponse for AppError {
    fn into_response(self) -> Response {
        (StatusCode::INTERNAL_SERVER_ERROR, self.0.to_string()).into_response()
    }
}
