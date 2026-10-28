//! GitHub OAuth for the desktop app and the web page (M2-003b, plan §4.2 "Auth", client-api §5.9).
//!
//! Two PKCE legs, both S256 only (RFC 7636), the hub in the middle:
//!
//! ```text
//! app/web --GET /v1/auth/github/authorize?redirect_uri&code_challenge&state--> hub
//! hub --302--> GitHub authorize (hub's own state + hub's own PKCE challenge)
//! GitHub --302--> hub /auth/github/callback?code&state
//! hub: consumes the state, exchanges the code with the client secret + the hub verifier,
//!      reads the GitHub user, maps it (by GitHub user id) to a hub user, accepts invites
//! hub --302--> redirect_uri?code=<hub code>&state=<the app's state>
//! app/web --POST /v1/auth/github/token {code, code_verifier, redirect_uri}--> hub: a session
//! ```
//!
//! This file is platform neutral (the HTTP to GitHub and to the client is in `github_routes`, wasm
//! only), so the whole flow, with the GitHub reply injected, runs in the native tests.
//!
//! Security rules kept here:
//! - `redirect_uri` is checked against an allowlist before anything is stored or redirected to:
//!   a loopback `http://127.0.0.1:<port>/callback` or `http://[::1]:<port>/callback` (RFC 8252
//!   §7.3; `localhost` is not accepted), or exactly the hub's own `<origin>/auth/github/callback`
//!   (the web page). A bad one is a `400` JSON answer, never a redirect.
//! - only `code_challenge_method=S256`; the app's `state` is required and echoed back.
//! - the `state` sent to GitHub is random, single use, 10 minutes, and only its SHA-256 is stored;
//!   it is bound to the hub's PKCE verifier for the GitHub leg and to the app's challenge.
//! - the hub's authorization code is random, single use, 2 minutes, hash only, and is traded for a
//!   session only together with the verifier of the challenge it was issued for.
//! - a hub user is found by the GitHub user id. An account with no hub user and no open invite
//!   gets nothing created.
//! - tokens, codes, verifiers and the client secret never reach an audit row or a log.

use base64::Engine as _;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use super::github_store as store;
use super::service::{BROWSER_SESSION_TTL_SECONDS, discard_member_session, issue_member_session};
use super::token::{ct_eq, hash_token, new_id, new_token};
use crate::audit::{self, Event};
use crate::db::{Db, DbError, Role};
use crate::devices::limits;
use crate::threads::ApiError;

/// The `state` sent to GitHub lives this long.
pub const STATE_TTL_MS: i64 = 10 * 60 * 1000;
/// The hub's authorization code lives this long.
pub const CODE_TTL_MS: i64 = 2 * 60 * 1000;
/// An invite lives this long.
pub const INVITE_TTL_MS: i64 = 14 * 24 * 60 * 60 * 1000;
/// Session lifetime for the desktop app (the web page uses the browser session lifetime).
const DESKTOP_SESSION_TTL_SECONDS: i64 = 30 * 24 * 60 * 60;
/// Rides in `oauth_codes.invite_ids` when the sign-in linked a pre-created user by login, so the
/// redeem can audit it (`auth.github.link`). Not an invite id (those start with `inv_`).
const LINKED_MARKER: &str = "@linked";
/// The same, for the bootstrap owner linked by `BOOTSTRAP_OWNER_GITHUB_LOGIN` (audited with
/// `by` = `bootstrap_owner_login`).
const BOOTSTRAP_LINKED_MARKER: &str = "@bootstrap";
/// The path GitHub redirects to; also the web page's `redirect_uri`.
pub const CALLBACK_PATH: &str = "/auth/github/callback";
const LOOPBACK_PATH: &str = "/callback";
const DEFAULT_OAUTH_BASE: &str = "https://github.com";
const DEFAULT_API_BASE: &str = "https://api.github.com";

fn db_failure(error: DbError) -> ApiError {
    ApiError::internal(format!("github oauth: {error}"))
}

fn invalid_grant(message: &str) -> ApiError {
    ApiError::new(400, "invalid_grant", message)
}

/// What the hub needs to talk to GitHub, read from the Worker's vars and secrets by the routes.
#[derive(Debug, Clone)]
pub struct GithubConfig {
    pub client_id: String,
    /// `GITHUB_API_BASE` (dev and tests point it at a mock). When set, authorize, token and user
    /// all live under it; unset means github.com for the first two and api.github.com for the user.
    pub api_base: Option<String>,
    /// The hub's public origin (`https://host`, no trailing slash).
    pub hub_origin: String,
    /// `BOOTSTRAP_OWNER_GITHUB_LOGIN` (lowercase), when the operator set a valid one. The first
    /// sign-in of this login links the bootstrap owner (see `finish_callback`); unset links nothing.
    pub bootstrap_login: Option<String>,
}

impl GithubConfig {
    pub fn authorize_url(&self) -> String {
        format!("{}/login/oauth/authorize", self.oauth_base())
    }

    pub fn token_url(&self) -> String {
        format!("{}/login/oauth/access_token", self.oauth_base())
    }

    pub fn user_url(&self) -> String {
        format!("{}/user", self.api_base_or_default())
    }

    /// The URL GitHub sends the browser back to (registered in the GitHub OAuth app).
    pub fn callback_url(&self) -> String {
        format!("{}{CALLBACK_PATH}", self.hub_origin)
    }

    fn oauth_base(&self) -> &str {
        self.api_base.as_deref().unwrap_or(DEFAULT_OAUTH_BASE)
    }

    fn api_base_or_default(&self) -> &str {
        self.api_base.as_deref().unwrap_or(DEFAULT_API_BASE)
    }
}

// ---- PKCE and small encodings --------------------------------------------------------------

/// The S256 challenge of a verifier: `BASE64URL(SHA256(verifier))` without padding (RFC 7636 §4.2).
pub fn s256_challenge(verifier: &str) -> String {
    URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()))
}

/// A verifier is 43-128 characters of `[A-Za-z0-9-._~]` (RFC 7636 §4.1).
pub fn is_valid_verifier(value: &str) -> bool {
    (43..=128).contains(&value.len()) && value.bytes().all(is_unreserved)
}

/// A challenge from S256 is exactly 43 base64url characters.
fn is_valid_challenge(value: &str) -> bool {
    value.len() == 43
        && value
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn is_unreserved(b: u8) -> bool {
    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~')
}

/// Percent-encodes everything but the unreserved characters.
pub fn pct_encode(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for b in value.bytes() {
        if is_unreserved(b) {
            out.push(char::from(b));
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Decodes `%XX` and `+` of a query value; `None` on a bad escape or non UTF-8.
pub fn pct_decode(value: &str) -> Option<String> {
    let bytes = value.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0;
    while i < bytes.len() {
        match bytes[i] {
            b'%' => {
                let hex = value.get(i + 1..i + 3)?;
                out.push(u8::from_str_radix(hex, 16).ok()?);
                i += 3;
            }
            b'+' => {
                out.push(b' ');
                i += 1;
            }
            other => {
                out.push(other);
                i += 1;
            }
        }
    }
    String::from_utf8(out).ok()
}

/// The `key=value` pairs of a query string (decoded). A pair with a bad escape is dropped.
pub fn parse_query(query: &str) -> Vec<(String, String)> {
    query
        .split('&')
        .filter(|pair| !pair.is_empty())
        .filter_map(|pair| {
            let (key, value) = pair.split_once('=').unwrap_or((pair, ""));
            Some((pct_decode(key)?, pct_decode(value)?))
        })
        .collect()
}

fn param<'a>(params: &'a [(String, String)], key: &str) -> Option<&'a str> {
    params
        .iter()
        .find(|(name, _)| name == key)
        .map(|(_, value)| value.as_str())
}

// ---- redirect allowlist --------------------------------------------------------------------

/// Where the app's redirect goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RedirectKind {
    /// The desktop app listening on a loopback port.
    Loopback,
    /// The hub's own web page.
    Web,
}

/// Checks `redirect_uri` against the allowlist; `None` when it is not on it.
pub fn classify_redirect(redirect_uri: &str, hub_origin: &str) -> Option<RedirectKind> {
    if redirect_uri.len() > 200 || !redirect_uri.bytes().all(|b| (0x21..0x7f).contains(&b)) {
        return None;
    }
    if redirect_uri == format!("{hub_origin}{CALLBACK_PATH}") {
        return Some(RedirectKind::Web);
    }
    // RFC 8252 §7.3: http, a loopback IP literal, any port, a fixed path; nothing else in the URI.
    let rest = redirect_uri.strip_prefix("http://")?;
    let (authority, path) = rest.split_once('/')?;
    if format!("/{path}") != LOOPBACK_PATH {
        return None;
    }
    let port = authority
        .strip_prefix("127.0.0.1:")
        .or_else(|| authority.strip_prefix("[::1]:"))?;
    let valid_port = !port.is_empty()
        && port.len() <= 5
        && port.bytes().all(|b| b.is_ascii_digit())
        && port.parse::<u32>().is_ok_and(|p| (1..=65535).contains(&p))
        && !port.starts_with('0');
    valid_port.then_some(RedirectKind::Loopback)
}

fn is_valid_app_state(value: &str) -> bool {
    (8..=128).contains(&value.len()) && value.bytes().all(is_unreserved)
}

// ---- step 1: authorize ---------------------------------------------------------------------

/// `GET /v1/auth/github/authorize?...`: validates the request, stores it and returns the GitHub
/// URL to redirect the browser to. Query: `redirect_uri`, `code_challenge`,
/// `code_challenge_method=S256`, `state`, optional `org_id` (the org the session is for).
///
/// Open to the world and every call writes a row, so it is limited per `client_ip`
/// (`CF-Connecting-IP`, `limits::check_authorize`; no IP means no limit): `429` + `Retry-After`.
pub async fn authorize(
    db: &impl Db,
    config: &GithubConfig,
    query: &[(String, String)],
    client_ip: Option<&str>,
    now_ms: i64,
) -> Result<String, ApiError> {
    if let Some(limited) = limits::check_authorize(db, client_ip, now_ms).await? {
        return Err(limited.into_api());
    }
    let bad = |message: &str| ApiError::invalid_request(message.to_owned());
    let redirect_uri =
        param(query, "redirect_uri").ok_or_else(|| bad("redirect_uri is required"))?;
    if classify_redirect(redirect_uri, &config.hub_origin).is_none() {
        return Err(bad(
            "redirect_uri is not allowed: use http://127.0.0.1:<port>/callback or the hub's own callback",
        ));
    }
    match param(query, "code_challenge_method") {
        Some("S256") => {}
        _ => return Err(bad("code_challenge_method must be S256")),
    }
    let challenge =
        param(query, "code_challenge").ok_or_else(|| bad("code_challenge is required"))?;
    if !is_valid_challenge(challenge) {
        return Err(bad("code_challenge must be a 43 character S256 challenge"));
    }
    let app_state = param(query, "state").ok_or_else(|| bad("state is required"))?;
    if !is_valid_app_state(app_state) {
        return Err(bad("state must be 8-128 characters of [A-Za-z0-9-._~]"));
    }
    let org_id = param(query, "org_id").filter(|id| !id.is_empty());
    if org_id.is_some_and(|id| !protocol::turn_event::is_valid_id(id)) {
        return Err(bad("org_id is not valid"));
    }

    let state = format!("ost_{}", &new_token()?[4..]);
    let verifier = new_token()?[4..].to_owned();
    let _ = store::purge_expired(db, now_ms).await;
    store::insert_request(
        db,
        &store::NewRequest {
            id: &new_id("oar")?,
            state_hash: &hash_token(&state),
            github_verifier: &verifier,
            redirect_uri,
            challenge,
            app_state,
            org_id,
            now_ms,
            expires_at_ms: now_ms + STATE_TTL_MS,
        },
    )
    .await
    .map_err(db_failure)?;

    Ok(format!(
        "{}?client_id={}&redirect_uri={}&state={}&scope=read%3Auser&allow_signup=false\
         &code_challenge={}&code_challenge_method=S256",
        config.authorize_url(),
        pct_encode(&config.client_id),
        pct_encode(&config.callback_url()),
        pct_encode(&state),
        s256_challenge(&verifier),
    ))
}

// ---- step 2: the GitHub callback -----------------------------------------------------------

/// A consumed authorize request: what the callback needs to finish.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Pending {
    /// The verifier to send to GitHub with the code.
    pub github_verifier: String,
    pub redirect_uri: String,
    pub challenge: String,
    pub app_state: String,
    pub org_id: Option<String>,
}

/// The hourly cron's work: drops expired authorize requests, codes and invites. Returns the rows
/// deleted `(requests, codes, invites)`.
pub async fn purge(db: &impl Db, now_ms: i64) -> Result<(u64, u64, u64), ApiError> {
    store::purge_expired(db, now_ms).await.map_err(db_failure)
}

/// Consumes the `state` GitHub sent back. `None` when it is unknown, used or expired (the caller
/// answers `400` and redirects nowhere: there is no trusted redirect target).
pub async fn begin_callback(
    db: &impl Db,
    state: &str,
    now_ms: i64,
) -> Result<Option<Pending>, ApiError> {
    if state.len() > 128 || !state.starts_with("ost_") {
        return Ok(None);
    }
    store::consume_request(db, &hash_token(state), now_ms)
        .await
        .map_err(db_failure)
}

/// The user GitHub vouches for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GithubUser {
    pub id: i64,
    pub login: String,
    pub name: Option<String>,
    pub email: Option<String>,
}

/// A GitHub login: 1-39 characters of `[A-Za-z0-9-]`, no leading or trailing hyphen.
pub fn is_valid_login(login: &str) -> bool {
    (1..=39).contains(&login.len())
        && login
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-')
        && !login.starts_with('-')
        && !login.ends_with('-')
}

/// Reads the `access_token` of GitHub's token endpoint reply (`Accept: application/json`).
pub fn parse_token_response(body: &str) -> Result<String, ApiError> {
    let value: Value = serde_json::from_str(body)
        .map_err(|_| ApiError::new(502, "bad_gateway", "GitHub's token reply is not JSON"))?;
    match value.get("access_token").and_then(Value::as_str) {
        Some(token) if !token.is_empty() => Ok(token.to_owned()),
        _ => Err(ApiError::new(
            502,
            "bad_gateway",
            "GitHub did not grant an access token",
        )),
    }
}

/// Reads `GET /user`: the numeric id, the login, and the optional public name and email.
pub fn parse_user(body: &str) -> Result<GithubUser, ApiError> {
    let bad = || ApiError::new(502, "bad_gateway", "GitHub's user reply is not usable");
    let value: Value = serde_json::from_str(body).map_err(|_| bad())?;
    let id = value
        .get("id")
        .and_then(Value::as_i64)
        .filter(|id| *id > 0)
        .ok_or_else(bad)?;
    let login = value
        .get("login")
        .and_then(Value::as_str)
        .filter(|login| is_valid_login(login))
        .ok_or_else(bad)?;
    let text = |key: &str| {
        value
            .get(key)
            .and_then(Value::as_str)
            .map(|s| s.chars().take(128).collect::<String>())
            .filter(|s| !s.trim().is_empty())
    };
    Ok(GithubUser {
        id,
        login: login.to_owned(),
        name: text("name"),
        email: text("email"),
    })
}

/// Where to send the browser at the end of the callback.
pub fn app_location(pending: &Pending, hub_origin: &str, params: &[(&str, &str)]) -> String {
    let query = params
        .iter()
        .map(|(key, value)| format!("{key}={}", pct_encode(value)))
        .collect::<Vec<_>>()
        .join("&");
    // The web page gets the result in the fragment, so it is never sent to a server or logged.
    if classify_redirect(&pending.redirect_uri, hub_origin) == Some(RedirectKind::Web) {
        format!("{hub_origin}/#/auth/github?{query}")
    } else {
        format!("{}?{query}", pending.redirect_uri)
    }
}

/// The redirect for a failed or refused sign-in (`error=access_denied`, the app's state echoed).
pub fn error_location(pending: &Pending, hub_origin: &str, error: &str) -> String {
    app_location(
        pending,
        hub_origin,
        &[("error", error), ("state", &pending.app_state)],
    )
}

/// After GitHub vouched for `user`: map them to a hub user, accept their invites, pick the org,
/// and return the app redirect carrying the hub's one-time code. A GitHub account with no hub
/// user and no open invite, or no membership in the requested org, is `Ok(access_denied
/// redirect)` and creates nothing.
pub async fn finish_callback(
    db: &impl Db,
    config: &GithubConfig,
    pending: &Pending,
    user: &GithubUser,
    now_ms: i64,
) -> Result<String, ApiError> {
    let denied = || error_location(pending, &config.hub_origin, "access_denied");
    let login = user.login.to_ascii_lowercase();

    // Rides in the code's invite list so the redeem can audit how the account was linked.
    let mut link_marker: Option<&str> = None;
    let mut hub_user = store::user_by_github_id(db, user.id)
        .await
        .map_err(db_failure)?;
    if hub_user.is_none() && config.bootstrap_login.as_deref() == Some(login.as_str()) {
        // The operator named this login for the bootstrap owner, so it links that owner whatever
        // their age: once, and only while the owner has no GitHub id (the link sets one, so a
        // later sign-in finds them by id). Only the bootstrap owner is reachable this way.
        hub_user = store::link_bootstrap_owner(db, user.id, &login)
            .await
            .map_err(db_failure)?;
        if hub_user.is_some() {
            link_marker = Some(BOOTSTRAP_LINKED_MARKER);
        }
    }
    if hub_user.is_none() {
        // A user an admin created with this login and no GitHub id yet is linked, once, and
        // only while they are younger than 7 days: an older one may belong to a login that was
        // renamed and re-registered, so that sign-in needs a fresh invite instead. The link is
        // audited when the code is redeemed (`LINKED_MARKER` in the code's invite list).
        hub_user = store::link_user_by_login(db, user.id, &login, now_ms)
            .await
            .map_err(db_failure)?;
        if hub_user.is_some() {
            link_marker = Some(LINKED_MARKER);
        }
    }
    let invites = store::open_invites(db, &login, now_ms)
        .await
        .map_err(db_failure)?;
    let user_id = match hub_user {
        Some(id) => {
            store::refresh_user(db, &id, user.id, &user.login)
                .await
                .map_err(db_failure)?;
            id
        }
        None if invites.is_empty() => return Ok(denied()),
        None => {
            let id = new_id("usr")?;
            let display = user.name.as_deref().unwrap_or(&user.login);
            store::create_user(db, &id, user, display)
                .await
                .map_err(db_failure)?;
            id
        }
    };

    let mut accepted = Vec::new();
    if let Some(marker) = link_marker {
        accepted.push(marker.to_owned());
    }
    for invite in &invites {
        if store::accept_invite(db, invite, &user_id, now_ms)
            .await
            .map_err(db_failure)?
        {
            accepted.push(invite.id.clone());
        }
    }

    let orgs = store::member_orgs(db, &user_id).await.map_err(db_failure)?;
    let org_id = match pending.org_id.as_deref() {
        Some(wanted) => orgs.iter().find(|org| org.as_str() == wanted),
        None => orgs.first(),
    };
    let Some(org_id) = org_id else {
        return Ok(denied());
    };

    let code = format!("oac_{}", &new_token()?[4..]);
    store::insert_code(
        db,
        &store::NewCode {
            id: &new_id("oac")?,
            code_hash: &hash_token(&code),
            user_id: &user_id,
            org_id,
            redirect_uri: &pending.redirect_uri,
            challenge: &pending.challenge,
            invite_ids: &accepted.join(","),
            now_ms,
            expires_at_ms: now_ms + CODE_TTL_MS,
        },
    )
    .await
    .map_err(db_failure)?;
    Ok(app_location(
        pending,
        &config.hub_origin,
        &[("code", &code), ("state", &pending.app_state)],
    ))
}

// ---- step 3: the token endpoint ------------------------------------------------------------

/// `POST /v1/auth/github/token {code, code_verifier, redirect_uri}` (no auth; the code and the
/// verifier are the proof). Any failure is `400 invalid_grant` and burns the code. Success is the
/// same body as the other browser sign-ins: `{token, expires_at, user, org, role}`.
pub async fn redeem(
    db: &impl Db,
    config: &GithubConfig,
    body: &Value,
    now_ms: i64,
    request_id: &str,
) -> Result<Value, ApiError> {
    let field = |key: &str| {
        body.get(key)
            .and_then(Value::as_str)
            .filter(|value| !value.is_empty() && value.len() <= 256)
            .ok_or_else(|| ApiError::invalid_request(format!("{key} is required")))
    };
    let code = field("code")?;
    let verifier = field("code_verifier")?;
    let redirect_uri = field("redirect_uri")?;
    if !is_valid_verifier(verifier) {
        return Err(invalid_grant("code_verifier is not valid"));
    }

    let hash = hash_token(code);
    let Some(row) = store::find_code(db, &hash).await.map_err(db_failure)? else {
        return Err(invalid_grant("this code is used, expired or unknown"));
    };
    // Single use: only one caller wins the claim, and a wrong verifier or redirect burns it too.
    if !store::claim_code(db, &hash, now_ms)
        .await
        .map_err(db_failure)?
    {
        return Err(invalid_grant("this code is used, expired or unknown"));
    }
    if redirect_uri != row.redirect_uri
        || !ct_eq(
            s256_challenge(verifier).as_bytes(),
            row.challenge.as_bytes(),
        )
    {
        return Err(invalid_grant(
            "redirect_uri or code_verifier does not match",
        ));
    }

    let loopback =
        classify_redirect(&row.redirect_uri, &config.hub_origin) == Some(RedirectKind::Loopback);
    let (device_name, ttl) = if loopback {
        ("desktop", DESKTOP_SESSION_TTL_SECONDS)
    } else {
        ("web", BROWSER_SESSION_TTL_SECONDS)
    };
    let Some(session) =
        issue_member_session(db, &row.user_id, &row.org_id, Role::Owner, device_name, ttl).await?
    else {
        return Err(invalid_grant(
            "this account is no longer a member of the org",
        ));
    };

    let login = store::github_login_of(db, &row.user_id)
        .await
        .map_err(db_failure)?;
    let mut events = vec![(
        "auth.github.signin",
        "session",
        session.session_id.clone(),
        json!({ "github_login": login, "client": device_name }),
    )];
    for invite_id in row.invite_ids.split(',').filter(|id| !id.is_empty()) {
        if invite_id == LINKED_MARKER || invite_id == BOOTSTRAP_LINKED_MARKER {
            let by = if invite_id == LINKED_MARKER {
                "login"
            } else {
                "bootstrap_owner_login"
            };
            events.push((
                "auth.github.link",
                "user",
                row.user_id.clone(),
                json!({ "github_login": login, "by": by }),
            ));
            continue;
        }
        events.push((
            "member.invite.accept",
            "invite",
            invite_id.to_owned(),
            json!({ "github_login": login }),
        ));
    }
    for (action, kind, target, detail) in &events {
        let event = Event {
            org_id: &row.org_id,
            actor_user_id: &row.user_id,
            actor_device_id: &session.device_id,
            action,
            target_kind: kind,
            target_id: target,
            request_id,
            detail: detail.clone(),
        };
        if let Err(error) = audit::record(db, &event).await {
            discard_member_session(db, &session).await;
            return Err(error);
        }
    }
    Ok(session.to_json())
}

/// A pending invite as the sign-in sees it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenInvite {
    pub id: String,
    pub org_id: String,
    pub role: Role,
}
