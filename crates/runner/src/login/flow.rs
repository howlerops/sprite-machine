//! The device-code flow of `sm login` (RFC 8628) against the hub's `/v1/devices/*` routes.
//!
//! The device code and the credential only ever travel in JSON bodies over https (plain http is
//! accepted for loopback hubs, local dev). The credential is never printed.

use std::fmt;
use std::future::Future;
use std::io::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use qrcode::QrCode;
use qrcode::render::unicode::Dense1x2;
use serde_json::{Value, json};

use super::store::{CredentialStore, Login};

/// RFC 8628 §3.5: `slow_down` adds 5 seconds when the server gives no new interval.
const SLOW_DOWN_STEP: Duration = Duration::from_secs(5);
/// Poll failures (network) tolerated in a row before giving up.
const MAX_TRANSIENT_FAILURES: u32 = 3;

/// Why the login ended without a credential.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoginError {
    /// The code expired before anyone approved it.
    Expired,
    /// Someone refused the pairing.
    Denied,
}

impl fmt::Display for LoginError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Expired => {
                f.write_str("the code expired before it was approved; run `sm login` again")
            }
            Self::Denied => f.write_str("the pairing was denied"),
        }
    }
}

impl std::error::Error for LoginError {}

/// How the flow waits between polls. Production sleeps; tests record the durations.
pub type Sleeper = Arc<dyn Fn(Duration) -> Pin<Box<dyn Future<Output = ()> + Send>> + Send + Sync>;

/// The real sleeper (`tokio::time::sleep`).
pub fn real_sleeper() -> Sleeper {
    Arc::new(|duration| Box::pin(tokio::time::sleep(duration)))
}

/// `https://...`, or `http://` for a loopback host only (the credential must not cross the
/// network in clear). Returns the URL without a trailing slash.
pub fn normalize_hub(hub: &str) -> Result<String> {
    let hub = hub.trim().trim_end_matches('/');
    let rest = if let Some(rest) = hub.strip_prefix("https://") {
        rest
    } else if let Some(rest) = hub.strip_prefix("http://") {
        let host = rest.split(['/', ':']).next().unwrap_or_default();
        let host = if rest.starts_with('[') { "[::1]" } else { host };
        if !matches!(host, "localhost" | "127.0.0.1" | "[::1]") {
            bail!("refusing plain http for {hub}: use https (http is for localhost only)");
        }
        rest
    } else {
        bail!("the hub URL must start with https:// (or http:// for localhost): {hub}");
    };
    if rest.is_empty() {
        bail!("the hub URL has no host: {hub}");
    }
    Ok(hub.to_owned())
}

/// The terminal QR for `data`, two modules per character row. `None` when `data` does not fit.
pub fn qr_text(data: &str) -> Option<String> {
    let code = QrCode::new(data.as_bytes()).ok()?;
    Some(
        code.render::<Dense1x2>()
            .dark_color(Dense1x2::Light)
            .light_color(Dense1x2::Dark)
            .build(),
    )
}

/// What `POST /v1/devices/code` answered.
struct Code {
    device_code: String,
    user_code: String,
    verification_uri: String,
    verification_uri_complete: Option<String>,
    expires_in: Duration,
    interval: Duration,
}

fn str_field(body: &Value, name: &str) -> Result<String> {
    body.get(name)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the hub's answer has no {name}"))
}

fn seconds(body: &Value, name: &str) -> Option<Duration> {
    body.get(name)
        .and_then(Value::as_u64)
        .map(Duration::from_secs)
}

async fn request_code(http: &reqwest::Client, hub: &str) -> Result<Code> {
    let response = http
        .post(format!("{hub}/v1/devices/code"))
        .json(&json!({}))
        .send()
        .await
        .context("could not reach the hub")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        bail!("the hub refused to start a pairing (HTTP {status}): {body}");
    }
    Ok(Code {
        device_code: str_field(&body, "device_code")?,
        user_code: str_field(&body, "user_code")?,
        verification_uri: str_field(&body, "verification_uri")?,
        verification_uri_complete: body
            .get("verification_uri_complete")
            .and_then(Value::as_str)
            .map(str::to_owned),
        expires_in: seconds(&body, "expires_in").unwrap_or(Duration::from_secs(600)),
        interval: seconds(&body, "interval")
            .filter(|interval| !interval.is_zero())
            .unwrap_or(SLOW_DOWN_STEP),
    })
}

/// One poll: `Ok(Some(login))` when approved, `Ok(None)` to keep waiting (the interval may have
/// changed through `interval`).
enum Polled {
    Granted(Login),
    Pending,
    SlowDown(Option<Duration>),
}

async fn poll_once(http: &reqwest::Client, hub: &str, device_code: &str) -> Result<Polled> {
    let response = http
        .post(format!("{hub}/v1/devices/token"))
        .json(&json!({ "device_code": device_code }))
        .send()
        .await
        .context("could not reach the hub")?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if status.is_success() {
        return Ok(Polled::Granted(Login {
            hub: hub.to_owned(),
            machine_id: str_field(&body, "machine_id")?,
            credential: str_field(&body, "credential")?,
        }));
    }
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => Ok(Polled::Pending),
        Some("slow_down") => Ok(Polled::SlowDown(seconds(&body, "interval"))),
        Some("expired_token") => Err(LoginError::Expired.into()),
        Some("access_denied") => Err(LoginError::Denied.into()),
        Some(other) => bail!("the hub ended the pairing: {other}"),
        None => bail!("unexpected answer from the hub (HTTP {status})"),
    }
}

/// Runs `sm login`: shows the code, polls until approved, saves the credential in `store`.
/// `out` is where the instructions go (stdout in the binary).
pub async fn login(
    hub: &str,
    store: &CredentialStore,
    out: &mut (impl Write + Send),
    sleep: &Sleeper,
) -> Result<Login> {
    let hub = normalize_hub(hub)?;
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building the HTTP client")?;
    let code = request_code(&http, &hub).await?;

    let link = code
        .verification_uri_complete
        .as_deref()
        .unwrap_or(&code.verification_uri);
    writeln!(
        out,
        "To pair this machine, open:\n\n    {}\n",
        code.verification_uri
    )?;
    writeln!(out, "and enter the code:\n\n    {}\n", code.user_code)?;
    if let Some(qr) = qr_text(link) {
        writeln!(out, "or scan this QR code:\n\n{qr}\n")?;
    }
    writeln!(
        out,
        "Waiting for approval (the code expires in {} min)...",
        code.expires_in.as_secs().div_ceil(60)
    )?;
    out.flush()?;

    let mut interval = code.interval;
    let mut waited = Duration::ZERO;
    let mut failures = 0u32;
    let login = loop {
        sleep(interval).await;
        waited += interval;
        match poll_once(&http, &hub, &code.device_code).await {
            Ok(Polled::Granted(login)) => break login,
            Ok(Polled::Pending) => failures = 0,
            Ok(Polled::SlowDown(next)) => {
                failures = 0;
                interval = next
                    .filter(|next| *next > interval)
                    .unwrap_or(interval + SLOW_DOWN_STEP);
            }
            Err(error) if error.downcast_ref::<LoginError>().is_some() => return Err(error),
            Err(error)
                if error
                    .chain()
                    .any(|e| e.downcast_ref::<reqwest::Error>().is_some()) =>
            {
                failures += 1;
                if failures >= MAX_TRANSIENT_FAILURES {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
        if waited > code.expires_in + interval {
            return Err(LoginError::Expired.into());
        }
    };

    let backend = store.save(&login)?;
    writeln!(
        out,
        "Logged in: machine {} (credential stored in {backend}).",
        login.machine_id
    )?;
    out.flush()?;
    Ok(login)
}
