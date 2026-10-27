//! M0-017 spike: the shape the hub crate will copy.
//!
//! - `fetch` hands every request to an axum `Router` (workers-rs `http` + `axum` features).
//! - `/counter/{name}` and `/ws/{name}` are forwarded unchanged to the `Room` Durable
//!   Object (SQLite storage, hibernatable WebSocket), see `room.rs`.
//! - `/d1/notes` uses the D1 binding, `/r2/{key}` the R2 binding.
//!
//! Bindings (`Env`) are `!Send`; axum wants `Send` state and handlers, so `Env` sits in a
//! `SendWrapper` and every handler that awaits a JS future is marked `#[worker::send]`.
//! Workers run one thread per isolate, so both are sound.

use axum::{
    body::Body,
    extract::{Path, Request, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, put},
    Json, Router,
};
use serde::{Deserialize, Serialize};
use tower_service::Service;
use worker::{event, send::SendWrapper, Context, Env, HttpRequest};

mod room;
pub use room::Room;

type AppState = SendWrapper<Env>;

fn router(env: Env) -> Router {
    Router::new()
        .route("/health", get(|| async { "ok" }))
        .route("/counter/{name}", get(to_room).post(to_room))
        .route("/ws/{name}", get(to_room))
        .route("/d1/notes", get(list_notes).post(add_note))
        .route("/r2/{key}", put(r2_put).get(r2_get))
        .with_state(SendWrapper::new(env))
}

#[event(fetch)]
async fn fetch(req: HttpRequest, env: Env, _ctx: Context) -> worker::Result<Response<Body>> {
    Ok(router(env).call(req).await?)
}

/// Error type for handlers: a worker error becomes a 500 with the message as body.
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

type AppResult<T> = Result<T, AppError>;

/// Forward the request as-is to the `Room` DO named in the path. This also carries
/// WebSocket upgrades: the DO's 101 response keeps its `WebSocket` in the response
/// extensions, and workers-rs puts it back on the JS `Response`.
#[worker::send]
async fn to_room(
    State(env): State<AppState>,
    Path(name): Path<String>,
    req: Request,
) -> AppResult<Response> {
    let stub = env
        .durable_object("ROOM")?
        .id_from_name(&name)?
        .get_stub()?;
    let req = worker::Request::try_from(req)?;
    let res = stub.fetch_with_request(req).await?;
    Ok(res.into())
}

#[derive(Serialize, Deserialize)]
struct Note {
    id: i64,
    body: String,
}

#[worker::send]
async fn add_note(State(env): State<AppState>, body: String) -> AppResult<Json<Note>> {
    let db = env.d1("DB")?;
    let note = db
        .prepare("INSERT INTO notes (body) VALUES (?1) RETURNING id, body")
        .bind(&[body.into()])?
        .first::<Note>(None)
        .await?
        .ok_or_else(|| worker::Error::from("insert returned no row"))?;
    Ok(Json(note))
}

#[worker::send]
async fn list_notes(State(env): State<AppState>) -> AppResult<Json<Vec<Note>>> {
    let db = env.d1("DB")?;
    let notes = db
        .prepare("SELECT id, body FROM notes ORDER BY id")
        .all()
        .await?
        .results::<Note>()?;
    Ok(Json(notes))
}

#[worker::send]
async fn r2_put(
    State(env): State<AppState>,
    Path(key): Path<String>,
    body: axum::body::Bytes,
) -> AppResult<StatusCode> {
    env.bucket("BUCKET")?
        .put(key, body.to_vec())
        .execute()
        .await?;
    Ok(StatusCode::CREATED)
}

#[worker::send]
async fn r2_get(State(env): State<AppState>, Path(key): Path<String>) -> AppResult<Response> {
    let Some(object) = env.bucket("BUCKET")?.get(key).execute().await? else {
        return Ok(StatusCode::NOT_FOUND.into_response());
    };
    let bytes = match object.body() {
        Some(body) => body.bytes().await?,
        None => Vec::new(),
    };
    Ok(bytes.into_response())
}
