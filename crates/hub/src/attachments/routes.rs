//! The attachment routes (wasm32 only, M2-009):
//!
//! | Route                                                        | Auth                                              |
//! | ------------------------------------------------------------ | ------------------------------------------------- |
//! | `POST /v1/attachments`                                       | session token, member; `Idempotency-Key`; audited |
//! | `PUT /v1/attachments/{org}/{id}/upload?size&mime&exp&sig`    | the signed URL only (no session)                  |
//! | `GET /v1/attachments/{org}/{id}?exp&sig`                     | the signed URL only (no session)                  |
//!
//! The two signed routes need no session: the URL is the credential, and its HMAC covers the
//! operation, the object, the expiry and (for a PUT) the size and type. They do not touch D1.
//! The signing key comes from the `ATTACHMENT_SIGNING_KEY` secret or var; without it the routes
//! fail closed. `scripts/hub-test.sh` passes a random per-run key as a `--var`.

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Request, State},
    http::{HeaderMap, HeaderValue, StatusCode, Uri, header},
    response::{IntoResponse, Response},
    routing::{get, post, put},
};
use serde_json::Value;
use worker::{Env, HttpMetadata};

use super::service::{Attachments, CreateRequest};
use super::sign::{KeyTooShort, Signer};
use super::store::{BlobError, Blobs, Stored};
use super::{MAX_BYTES, SIGNING_KEY_VAR};
use crate::auth::routes::error_response;
use crate::auth::service::parse_body;
use crate::auth::{authenticate, dev_fixtures_enabled};
use crate::db::d1::D1;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

/// The R2 binding of the attachment bytes (wrangler.toml `[[r2_buckets]]`).
const BLOBS_BINDING: &str = "BLOBS";

/// The `Idempotency-Key` header (client-api.md §3).
const IDEMPOTENCY_HEADER: &str = "idempotency-key";

/// Largest `POST /v1/attachments` body. The body is a few fields of JSON.
const MAX_CREATE_BODY_BYTES: usize = 64 * 1024;

/// The attachment routes; merged into the Worker router by `worker.rs`.
pub fn attachment_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/attachments", post(create))
        .route("/v1/attachments/{org_id}/{attachment_id}", get(content))
        .route(
            "/v1/attachments/{org_id}/{attachment_id}/upload",
            put(upload),
        )
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
        .get(header::AUTHORIZATION)
        .and_then(|value| value.to_str().ok())
        .map(str::to_owned)
}

fn request_id_of(headers: &HeaderMap) -> String {
    let incoming = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    request_id(incoming, now_ms())
}

fn now_ms() -> u64 {
    worker::Date::now().as_millis()
}

fn database(env: &Env) -> Result<worker::D1Database, ApiError> {
    env.d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))
}

/// The hub's own origin, as the request addressed it: the signed URLs are built on it.
fn origin_of(uri: &Uri) -> Result<String, ApiError> {
    let authority = uri
        .authority()
        .ok_or_else(|| ApiError::internal("attachments: the request has no host"))?;
    Ok(format!(
        "{}://{authority}",
        uri.scheme_str().unwrap_or("https")
    ))
}

fn signer(env: &Env) -> Result<Signer, ApiError> {
    let configured = env
        .secret(SIGNING_KEY_VAR)
        .map(|secret| secret.to_string())
        .or_else(|_| env.var(SIGNING_KEY_VAR).map(|var| var.to_string()))
        .ok();
    let Some(key) = configured else {
        return Err(ApiError::internal(format!("{SIGNING_KEY_VAR} is not set")));
    };
    Signer::new(key.as_bytes()).map_err(|KeyTooShort| {
        ApiError::internal(format!("{SIGNING_KEY_VAR} must be at least 32 bytes"))
    })
}

/// The R2 bucket behind the [`Blobs`] trait.
struct R2(worker::Bucket);

fn bucket(env: &Env) -> Result<R2, ApiError> {
    env.bucket(BLOBS_BINDING)
        .map(R2)
        .map_err(|error| ApiError::internal(format!("R2 binding {BLOBS_BINDING}: {error}")))
}

fn blob_error(error: worker::Error) -> BlobError {
    BlobError(error.to_string())
}

impl Blobs for R2 {
    async fn put(&self, key: &str, bytes: Vec<u8>, content_type: &str) -> Result<(), BlobError> {
        let metadata = HttpMetadata {
            content_type: Some(content_type.to_owned()),
            ..HttpMetadata::default()
        };
        self.0
            .put(key, bytes)
            .http_metadata(metadata)
            .execute()
            .await
            .map(drop)
            .map_err(blob_error)
    }

    async fn get(&self, key: &str) -> Result<Option<Stored>, BlobError> {
        let Some(object) = self.0.get(key).execute().await.map_err(blob_error)? else {
            return Ok(None);
        };
        let content_type = object
            .http_metadata()
            .content_type
            .unwrap_or_else(|| "application/octet-stream".to_owned());
        let bytes = match object.body() {
            Some(body) => body.bytes().await.map_err(blob_error)?,
            None => Vec::new(),
        };
        Ok(Some(Stored {
            bytes,
            content_type,
        }))
    }
}

fn respond(result: Result<Value, ApiError>) -> Response {
    match result {
        Ok(body) => (StatusCode::CREATED, Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

/// `POST /v1/attachments`: authenticate (member is checked by the service), read the body, and
/// answer 201 with the signed URLs.
#[worker::send]
async fn create(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (parts, body) = req.into_parts();
        let database = database(&env)?;
        let db = D1::new(&database);
        let principal = authenticate(
            &db,
            authorization(&parts.headers).as_deref(),
            dev_fixtures(&env),
        )
        .await?;
        let origin = origin_of(&parts.uri)?;
        let bytes = axum::body::to_bytes(body, MAX_CREATE_BODY_BYTES)
            .await
            .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
        let body = parse_body(&bytes)?;
        let idempotency_key = parts
            .headers
            .get(IDEMPOTENCY_HEADER)
            .and_then(|value| value.to_str().ok())
            .map(str::to_owned);
        let request_id = request_id_of(&parts.headers);
        let blobs = bucket(&env)?;
        let signer = signer(&env)?;
        let request = CreateRequest {
            origin: &origin,
            idempotency_key: idempotency_key.as_deref(),
            body: &body,
            request_id: &request_id,
        };
        Attachments::new(&blobs, &signer)
            .create(&db, &principal, &request, now_ms())
            .await
    }
    .await;
    respond(result)
}

/// `PUT /v1/attachments/{org}/{id}/upload?...`: the signature and expiry are checked before the
/// body is read. Above 20 MB is 413; a body of another size is 400.
#[worker::send]
async fn upload(
    State(env): State<AppState>,
    Path((org_id, attachment_id)): Path<(String, String)>,
    req: Request,
) -> Response {
    let result = async {
        let signer = signer(&env)?;
        let blobs = bucket(&env)?;
        let (parts, body) = req.into_parts();
        let service = Attachments::new(&blobs, &signer);
        let grant = service.check_upload(now_ms(), &org_id, &attachment_id, parts.uri.query())?;
        let bytes = axum::body::to_bytes(body, MAX_BYTES as usize)
            .await
            .map_err(|_| ApiError::new(413, "invalid_request", "an attachment is at most 20 MB"))?;
        service.store_upload(&grant, Vec::from(bytes)).await?;
        Ok::<Response, ApiError>(StatusCode::NO_CONTENT.into_response())
    }
    .await;
    result.unwrap_or_else(|error| error_response(&error))
}

/// `GET /v1/attachments/{org}/{id}?exp&sig`: the stored bytes with their content type.
#[worker::send]
async fn content(
    State(env): State<AppState>,
    Path((org_id, attachment_id)): Path<(String, String)>,
    req: Request,
) -> Response {
    let result = async {
        let signer = signer(&env)?;
        let blobs = bucket(&env)?;
        let stored = Attachments::new(&blobs, &signer)
            .content(now_ms(), &org_id, &attachment_id, req.uri().query())
            .await?;
        let content_type = HeaderValue::from_str(&stored.content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream"));
        let mut response = Body::from(stored.bytes).into_response();
        response
            .headers_mut()
            .insert(header::CONTENT_TYPE, content_type);
        response.headers_mut().insert(
            header::HeaderName::from_static("x-content-type-options"),
            HeaderValue::from_static("nosniff"),
        );
        Ok::<Response, ApiError>(response)
    }
    .await;
    result.unwrap_or_else(|error| error_response(&error))
}
