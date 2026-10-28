//! GitHub sign-in for the desktop (client-api.md §5.9): authorization code + PKCE (RFC 7636,
//! S256 only) through the system browser, with a loopback redirect (RFC 8252 §7.3).
//!
//! Plain Rust, no GPUI: [`Pkce`], [`authorize_url`], the [`LoopbackListener`] that receives
//! `http://127.0.0.1:<port>/callback?code&state`, and [`exchange`] (`POST /v1/auth/github/token`).
//! [`sign_in`] strings them together; the browser launch goes through [`Browser`].

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use rand::RngCore as _;
use serde_json::json;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};
use tokio::net::TcpListener;

use super::auth::{LoginFailure, SessionGrant, api_url};
use super::browser::Browser;

/// A PKCE verifier and its S256 challenge.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pkce {
    pub verifier: String,
    pub challenge: String,
}

impl Pkce {
    /// 32 random bytes -> 43-character base64url verifier (RFC 7636 §4.1).
    pub fn generate() -> Self {
        Self::from_verifier(random_token(32))
    }

    pub fn from_verifier(verifier: String) -> Self {
        let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
        Self {
            verifier,
            challenge,
        }
    }
}

/// `n` random bytes as unpadded base64url (a valid `state`: `[A-Za-z0-9-_]`, 43 chars for 32).
pub fn random_token(n: usize) -> String {
    let mut bytes = vec![0u8; n];
    rand::rng().fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

/// The URL the system browser opens: `GET /v1/auth/github/authorize?...`.
pub fn authorize_url(
    hub: &str,
    redirect_uri: &str,
    pkce: &Pkce,
    state: &str,
    org_id: Option<&str>,
) -> String {
    let mut url = url::Url::parse(&api_url(hub, "auth/github/authorize"))
        .unwrap_or_else(|_| url::Url::parse("http://invalid.invalid/").expect("static url"));
    {
        let mut query = url.query_pairs_mut();
        query
            .append_pair("redirect_uri", redirect_uri)
            .append_pair("code_challenge", &pkce.challenge)
            .append_pair("code_challenge_method", "S256")
            .append_pair("state", state);
        if let Some(org_id) = org_id {
            query.append_pair("org_id", org_id);
        }
    }
    url.into()
}

/// What the loopback request carried.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Callback {
    Code(String),
    /// `error=access_denied` (or any other `error`) with the right state.
    Error(String),
    /// Not our request: wrong path or wrong/missing state. Answered and ignored.
    Ignored,
}

/// Interpret the first line of an HTTP request (`GET /callback?code=..&state=.. HTTP/1.1`).
pub fn parse_callback(request_line: &str, expected_state: &str) -> Callback {
    let mut parts = request_line.split_whitespace();
    if parts.next() != Some("GET") {
        return Callback::Ignored;
    }
    let Some(target) = parts.next() else {
        return Callback::Ignored;
    };
    let Ok(url) = url::Url::parse(&format!("http://127.0.0.1{target}")) else {
        return Callback::Ignored;
    };
    if url.path() != "/callback" {
        return Callback::Ignored;
    }
    let get = |name: &str| {
        url.query_pairs()
            .find(|(key, _)| key == name)
            .map(|(_, value)| value.into_owned())
    };
    if get("state").as_deref() != Some(expected_state) {
        return Callback::Ignored;
    }
    if let Some(error) = get("error") {
        return Callback::Error(error);
    }
    match get("code") {
        Some(code) if !code.is_empty() => Callback::Code(code),
        _ => Callback::Ignored,
    }
}

/// Listens on `127.0.0.1:<ephemeral port>` for the one redirect.
pub struct LoopbackListener {
    listener: TcpListener,
    port: u16,
}

const DONE_PAGE: &str = "<!doctype html><meta charset=utf-8><title>Sprite Machine</title>\
<p>You can close this tab and return to Sprite Machine.</p>";

impl LoopbackListener {
    pub async fn bind() -> std::io::Result<Self> {
        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let port = listener.local_addr()?.port();
        Ok(Self { listener, port })
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// The `redirect_uri` registered with the hub for this sign-in.
    pub fn redirect_uri(&self) -> String {
        format!("http://127.0.0.1:{}/callback", self.port)
    }

    /// Wait for the callback. Stray requests (other paths, wrong state) get an error page and
    /// do not end the wait. Stops on `cancel` or after `timeout`.
    pub async fn wait_for_callback(
        &self,
        expected_state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, LoginFailure> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::SeqCst) {
                return Err(LoginFailure::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LoginFailure::Timeout);
            }
            let accepted =
                tokio::time::timeout(Duration::from_millis(100), self.listener.accept()).await;
            let Ok(Ok((mut stream, _))) = accepted else {
                continue;
            };
            let Some(line) = read_request_line(&mut stream).await else {
                continue;
            };
            let outcome = parse_callback(&line, expected_state);
            let (status, body) = match outcome {
                Callback::Code(_) | Callback::Error(_) => ("200 OK", DONE_PAGE),
                Callback::Ignored => ("400 Bad Request", "Unexpected request."),
            };
            let response = format!(
                "HTTP/1.1 {status}\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}",
                body.len()
            );
            let _ = stream.write_all(response.as_bytes()).await;
            let _ = stream.shutdown().await;
            match outcome {
                Callback::Code(code) => return Ok(code),
                Callback::Error(error) if error == "access_denied" => {
                    return Err(LoginFailure::Denied);
                }
                Callback::Error(error) => return Err(LoginFailure::Hub(error)),
                Callback::Ignored => {}
            }
        }
    }
}

async fn read_request_line(stream: &mut tokio::net::TcpStream) -> Option<String> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let read = async {
        while buffer.len() < 8192 && !buffer.windows(2).any(|pair| pair == b"\r\n") {
            match stream.read(&mut chunk).await {
                Ok(0) | Err(_) => break,
                Ok(n) => buffer.extend_from_slice(&chunk[..n]),
            }
        }
    };
    tokio::time::timeout(Duration::from_secs(2), read)
        .await
        .ok()?;
    let text = String::from_utf8_lossy(&buffer).into_owned();
    text.split("\r\n").next().map(str::to_string)
}

/// `POST /v1/auth/github/token { code, code_verifier, redirect_uri }` -> the session.
pub async fn exchange(
    http: &reqwest::Client,
    hub: &str,
    code: &str,
    verifier: &str,
    redirect_uri: &str,
) -> Result<SessionGrant, LoginFailure> {
    let response = http
        .post(api_url(hub, "auth/github/token"))
        .json(&json!({ "code": code, "code_verifier": verifier, "redirect_uri": redirect_uri }))
        .send()
        .await
        .map_err(|error| LoginFailure::Network(error.to_string()))?;
    let status = response.status();
    let body: serde_json::Value = response.json().await.unwrap_or(serde_json::Value::Null);
    if status.is_success() {
        return SessionGrant::from_json(&body)
            .ok_or_else(|| LoginFailure::Hub("answer without a token".to_string()));
    }
    let reason = body
        .get("error")
        .and_then(|value| value.as_str())
        .map(str::to_string)
        .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
    Err(LoginFailure::Hub(reason))
}

/// Settings for one GitHub sign-in.
#[derive(Debug, Clone)]
pub struct GithubSignIn {
    pub hub: String,
    pub org_id: Option<String>,
    pub timeout: Duration,
}

/// The full flow: bind the loopback port, open the browser at the authorize URL, wait for the
/// redirect, exchange the code. `on_browser_open` runs once the browser was launched (so the UI
/// can switch to "finish in your browser").
pub async fn sign_in(
    settings: &GithubSignIn,
    browser: &dyn Browser,
    http: &reqwest::Client,
    cancel: &AtomicBool,
    on_browser_open: impl FnOnce(),
) -> Result<SessionGrant, LoginFailure> {
    let listener = LoopbackListener::bind()
        .await
        .map_err(|error| LoginFailure::Network(error.to_string()))?;
    let redirect_uri = listener.redirect_uri();
    let pkce = Pkce::generate();
    let state = random_token(32);
    let url = authorize_url(
        &settings.hub,
        &redirect_uri,
        &pkce,
        &state,
        settings.org_id.as_deref(),
    );
    browser.open(&url).map_err(LoginFailure::Browser)?;
    on_browser_open();
    let code = listener
        .wait_for_callback(&state, settings.timeout, cancel)
        .await?;
    exchange(http, &settings.hub, &code, &pkce.verifier, &redirect_uri).await
}
