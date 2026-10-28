//! The GitHub sign-in routes (wasm32 only; the logic is in `github.rs`):
//!
//! | Route                                   | Auth                                              |
//! | --------------------------------------- | ------------------------------------------------- |
//! | `GET /v1/auth/github/authorize?...`     | none; `302` to GitHub                             |
//! | `GET /auth/github/callback?code&state`  | none; GitHub's redirect, `302` to the app         |
//! | `POST /v1/auth/github/token`            | none; the hub code + PKCE verifier are the proof  |
//!
//! Config: `GITHUB_CLIENT_ID` (var), `GITHUB_CLIENT_SECRET` (a Worker secret, never in config),
//! optional `GITHUB_API_BASE` (dev and tests point it at a mock GitHub) and optional
//! `HUB_PUBLIC_URL` (the hub's origin; default: the request's `Host`).

use axum::{
    Router,
    extract::{Request, State},
    http::{
        HeaderMap, HeaderValue, StatusCode,
        header::{CACHE_CONTROL, LOCATION, RETRY_AFTER},
    },
    response::{IntoResponse, Response},
    routing::{get, post},
};
use worker::wasm_bindgen::JsValue;
use worker::{Env, Fetch, Headers, Method, RequestInit};

use super::github::{self, GithubConfig, GithubUser, pct_encode};
use super::routes::error_response;
use super::service::parse_body;
use crate::db::d1::D1;
use crate::devices::limits;
use crate::router::{REQUEST_ID_HEADER, request_id};
use crate::threads::ApiError;
use crate::worker::AppState;

const MAX_BODY_BYTES: usize = 16 * 1024;
const USER_AGENT: &str = "sprite-machine-hub";

/// The GitHub sign-in routes; merged into the Worker router by `worker.rs`.
pub fn github_routes() -> Router<AppState> {
    Router::new()
        .route("/v1/auth/github/authorize", get(authorize))
        .route(github::CALLBACK_PATH, get(callback))
        .route("/v1/auth/github/token", post(token))
}

/// The cron trigger's work: purge expired authorize requests, hub codes and invites.
pub async fn purge_scheduled(env: &Env) {
    let result = async {
        let database = database(env)?;
        github::purge(&D1::new(&database), now_ms()).await
    }
    .await;
    match result {
        Ok((requests, codes, invites)) => worker::console_log!(
            "github purge: {requests} requests, {codes} codes, {invites} invites"
        ),
        Err(error) => worker::console_log!("github purge failed: {}", error.message),
    }
}

fn now_ms() -> i64 {
    i64::try_from(worker::Date::now().as_millis()).unwrap_or(i64::MAX)
}

fn database(env: &Env) -> Result<worker::D1Database, ApiError> {
    env.d1("DB")
        .map_err(|error| ApiError::internal(format!("D1 binding DB: {error}")))
}

fn var(env: &Env, name: &str) -> Option<String> {
    env.var(name)
        .ok()
        .map(|value| value.to_string())
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
}

/// The client secret: a Worker secret (or, locally, a `--var`).
fn client_secret(env: &Env) -> Option<String> {
    env.secret("GITHUB_CLIENT_SECRET")
        .map(|secret| secret.to_string())
        .ok()
        .filter(|secret| !secret.trim().is_empty())
        .or_else(|| var(env, "GITHUB_CLIENT_SECRET"))
}

fn request_id_of(headers: &HeaderMap) -> String {
    let incoming = headers
        .get(REQUEST_ID_HEADER)
        .and_then(|value| value.to_str().ok());
    request_id(incoming, worker::Date::now().as_millis())
}

fn hub_origin(env: &Env, headers: &HeaderMap) -> String {
    if let Some(url) = var(env, "HUB_PUBLIC_URL") {
        return url.trim_end_matches('/').to_owned();
    }
    let host = headers
        .get("host")
        .and_then(|value| value.to_str().ok())
        .unwrap_or("localhost");
    let local =
        host.starts_with("127.0.0.1") || host.starts_with("localhost") || host.starts_with("[::1]");
    format!("{}://{host}", if local { "http" } else { "https" })
}

fn config(env: &Env, headers: &HeaderMap) -> Result<GithubConfig, ApiError> {
    let client_id = var(env, "GITHUB_CLIENT_ID").ok_or_else(not_configured)?;
    Ok(GithubConfig {
        client_id,
        api_base: var(env, "GITHUB_API_BASE").map(|base| base.trim_end_matches('/').to_owned()),
        hub_origin: hub_origin(env, headers),
    })
}

fn not_configured() -> ApiError {
    ApiError::new(
        503,
        "not_configured",
        "GitHub sign-in is not configured on this hub",
    )
}

/// A `302` that no cache or referrer may keep: the URL can carry a one-time code.
fn redirect(location: &str) -> Response {
    let mut response = StatusCode::FOUND.into_response();
    if let Ok(value) = HeaderValue::from_str(location) {
        response.headers_mut().insert(LOCATION, value);
    }
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

#[worker::send]
async fn authorize(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let config = config(&env, req.headers())?;
        let query = github::parse_query(req.uri().query().unwrap_or(""));
        let database = database(&env)?;
        // Cloudflare sets this from the connection; the rate limit is per IP when it is present.
        let client_ip = req
            .headers()
            .get("cf-connecting-ip")
            .and_then(|value| value.to_str().ok());
        github::authorize(&D1::new(&database), &config, &query, client_ip, now_ms()).await
    }
    .await;
    match result {
        Ok(location) => redirect(&location),
        Err(error) => {
            let mut response = error_response(&error);
            if let Some(seconds) = limits::retry_after(&error)
                && let Ok(value) = HeaderValue::from_str(&seconds.to_string())
            {
                response.headers_mut().insert(RETRY_AFTER, value);
            }
            response
        }
    }
}

#[worker::send]
async fn callback(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let config = config(&env, req.headers())?;
        let query = github::parse_query(req.uri().query().unwrap_or(""));
        let get = |key: &str| {
            query
                .iter()
                .find(|(name, _)| name == key)
                .map(|(_, value)| value.clone())
        };
        let database = database(&env)?;
        let db = D1::new(&database);
        let now = now_ms();
        let state = get("state").unwrap_or_default();
        let Some(pending) = github::begin_callback(&db, &state, now).await? else {
            // No trusted redirect target without a valid state: a plain error, no redirect.
            return Err(ApiError::invalid_request(
                "this sign-in is unknown, used or expired; start again",
            ));
        };
        if let Some(error) = get("error") {
            // The user declined at GitHub (or GitHub refused): tell the app, name nothing else.
            let reason = if error == "access_denied" {
                "access_denied"
            } else {
                "server_error"
            };
            return Ok(github::error_location(&pending, &config.hub_origin, reason));
        }
        let Some(code) = get("code").filter(|code| !code.is_empty() && code.len() <= 512) else {
            return Err(ApiError::invalid_request("code is required"));
        };
        // GitHub or the network failing is the app's `server_error`, not a hub 5xx page.
        let user = match exchange(&env, &config, &code, &pending.github_verifier).await {
            Ok(user) => user,
            Err(error) => {
                worker::console_log!("github exchange failed: {}", error.code);
                return Ok(github::error_location(
                    &pending,
                    &config.hub_origin,
                    "server_error",
                ));
            }
        };
        github::finish_callback(&db, &config, &pending, &user, now).await
    }
    .await;
    match result {
        Ok(location) => redirect(&location),
        Err(error) => error_response(&error),
    }
}

#[worker::send]
async fn token(State(env): State<AppState>, req: Request) -> Response {
    let result = async {
        let (parts, body) = req.into_parts();
        let bytes = axum::body::to_bytes(body, MAX_BODY_BYTES)
            .await
            .map_err(|_| ApiError::invalid_request("request body is too large or unreadable"))?;
        let body = parse_body(&bytes)?;
        let config = config(&env, &parts.headers)?;
        let database = database(&env)?;
        github::redeem(
            &D1::new(&database),
            &config,
            &body,
            now_ms(),
            &request_id_of(&parts.headers),
        )
        .await
    }
    .await;
    match result {
        Ok(body) => (StatusCode::CREATED, axum::Json(body)).into_response(),
        Err(error) => error_response(&error),
    }
}

// ---- outbound HTTP to GitHub ---------------------------------------------------------------

fn bad_gateway(message: &str) -> ApiError {
    ApiError::new(502, "bad_gateway", message.to_owned())
}

/// Trades the GitHub `code` (with the client secret and the hub's PKCE verifier) for an access
/// token, then reads the user it belongs to. The access token is used once and dropped.
async fn exchange(
    env: &Env,
    config: &GithubConfig,
    code: &str,
    verifier: &str,
) -> Result<GithubUser, ApiError> {
    let secret = client_secret(env).ok_or_else(not_configured)?;
    let form = format!(
        "client_id={}&client_secret={}&code={}&redirect_uri={}&code_verifier={}",
        pct_encode(&config.client_id),
        pct_encode(&secret),
        pct_encode(code),
        pct_encode(&config.callback_url()),
        pct_encode(verifier),
    );
    let headers = Headers::new();
    let set = |name: &str, value: &str| {
        headers
            .set(name, value)
            .map_err(|_| bad_gateway("cannot build the GitHub request"))
    };
    set("Accept", "application/json")?;
    set("Content-Type", "application/x-www-form-urlencoded")?;
    set("User-Agent", USER_AGENT)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from_str(&form)));
    let reply = send(&config.token_url(), &init).await?;
    let access_token = github::parse_token_response(&reply)?;

    let headers = Headers::new();
    let set = |name: &str, value: &str| {
        headers
            .set(name, value)
            .map_err(|_| bad_gateway("cannot build the GitHub request"))
    };
    set("Accept", "application/vnd.github+json")?;
    set("Authorization", &format!("Bearer {access_token}"))?;
    set("User-Agent", USER_AGENT)?;
    let mut init = RequestInit::new();
    init.with_method(Method::Get).with_headers(headers);
    github::parse_user(&send(&config.user_url(), &init).await?)
}

/// One GitHub call; a non-2xx answer is a `502` and its body is dropped (it is never logged).
async fn send(url: &str, init: &RequestInit) -> Result<String, ApiError> {
    let request = worker::Request::new_with_init(url, init)
        .map_err(|_| bad_gateway("cannot build the GitHub request"))?;
    let mut response = Fetch::Request(request)
        .send()
        .await
        .map_err(|_| bad_gateway("cannot reach GitHub"))?;
    if !(200..300).contains(&response.status_code()) {
        return Err(bad_gateway("GitHub refused the request"));
    }
    response
        .text()
        .await
        .map_err(|_| bad_gateway("cannot read GitHub's reply"))
}
