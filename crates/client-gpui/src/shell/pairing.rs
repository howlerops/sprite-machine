//! Login by device-code pairing (client-api.md §5.7 flow B, RFC 8628): the desktop asks the hub
//! for a code, shows it as a QR code (plus the typed code) for a phone that is already signed in,
//! and polls until the phone approves it. Plain Rust; the polling loop takes its sleep function
//! so tests do not wait.

use std::future::Future;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use qrcode::{Color, QrCode};
use serde_json::{Value, json};

use super::auth::{LoginFailure, SessionGrant, api_url};

/// `POST /v1/auth/device/code` answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_uri: String,
    pub verification_uri_complete: Option<String>,
    pub interval: u64,
    pub expires_in: u64,
}

impl DeviceCode {
    pub fn from_json(body: &Value) -> Option<Self> {
        let text = |key: &str| body.get(key).and_then(Value::as_str).map(str::to_string);
        Some(Self {
            device_code: text("device_code")?,
            user_code: text("user_code")?,
            verification_uri: text("verification_uri")?,
            verification_uri_complete: text("verification_uri_complete"),
            interval: body.get("interval").and_then(Value::as_u64).unwrap_or(5),
            expires_in: body
                .get("expires_in")
                .and_then(Value::as_u64)
                .unwrap_or(600),
        })
    }

    /// What the QR code encodes: the link with the code filled in when the hub gave one.
    pub fn qr_target(&self) -> &str {
        self.verification_uri_complete
            .as_deref()
            .unwrap_or(&self.verification_uri)
    }
}

/// One poll of `POST /v1/auth/device/token`, classified (RFC 8628 §3.5).
#[derive(Debug, Clone, PartialEq)]
pub enum PollOutcome {
    Pending,
    /// Poll slower; the hub may name the new interval.
    SlowDown(Option<u64>),
    Expired,
    Denied,
    Granted(Box<SessionGrant>),
    Failed(LoginFailure),
}

/// Seconds added to the interval on `slow_down` when the hub does not name one (RFC 8628 §3.5).
pub const SLOW_DOWN_STEP_SECONDS: u64 = 5;

pub fn classify_poll(status: u16, body: &Value) -> PollOutcome {
    if (200..300).contains(&status) {
        return match SessionGrant::from_json(body) {
            Some(grant) => PollOutcome::Granted(Box::new(grant)),
            None => PollOutcome::Failed(LoginFailure::Hub("answer without a token".to_string())),
        };
    }
    match body.get("error").and_then(Value::as_str) {
        Some("authorization_pending") => PollOutcome::Pending,
        Some("slow_down") => PollOutcome::SlowDown(body.get("interval").and_then(Value::as_u64)),
        Some("expired_token") => PollOutcome::Expired,
        Some("access_denied") => PollOutcome::Denied,
        Some(other) => PollOutcome::Failed(LoginFailure::Hub(other.to_string())),
        None => PollOutcome::Failed(LoginFailure::Hub(format!("HTTP {status}"))),
    }
}

/// The poll cadence: the interval grows on `slow_down`, and the wait ends at `expires_in`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PollSchedule {
    interval: u64,
    waited: u64,
    expires_in: u64,
}

impl PollSchedule {
    pub fn new(code: &DeviceCode) -> Self {
        Self {
            interval: code.interval.max(1),
            waited: 0,
            expires_in: code.expires_in,
        }
    }

    pub fn interval(&self) -> Duration {
        Duration::from_secs(self.interval)
    }

    pub fn slow_down(&mut self, named: Option<u64>) {
        self.interval = match named {
            Some(interval) if interval > self.interval => interval,
            _ => self.interval + SLOW_DOWN_STEP_SECONDS,
        };
    }

    /// Record one wait; `false` once the code's lifetime is used up.
    pub fn wait_done(&mut self) -> bool {
        self.waited += self.interval;
        self.waited <= self.expires_in
    }
}

/// `POST /v1/auth/device/code`.
pub async fn request_code(http: &reqwest::Client, hub: &str) -> Result<DeviceCode, LoginFailure> {
    let response = http
        .post(api_url(hub, "auth/device/code"))
        .json(&json!({}))
        .send()
        .await
        .map_err(|error| LoginFailure::Network(error.to_string()))?;
    let status = response.status();
    let body: Value = response.json().await.unwrap_or(Value::Null);
    if !status.is_success() {
        let reason = body
            .get("error")
            .and_then(Value::as_str)
            .map(str::to_string)
            .unwrap_or_else(|| format!("HTTP {}", status.as_u16()));
        return Err(LoginFailure::Hub(reason));
    }
    DeviceCode::from_json(&body)
        .ok_or_else(|| LoginFailure::Hub("malformed device code answer".to_string()))
}

/// `POST /v1/auth/device/token { device_code }`.
pub async fn poll_once(http: &reqwest::Client, hub: &str, device_code: &str) -> PollOutcome {
    let response = http
        .post(api_url(hub, "auth/device/token"))
        .json(&json!({ "device_code": device_code }))
        .send()
        .await;
    match response {
        Err(error) => PollOutcome::Failed(LoginFailure::Network(error.to_string())),
        Ok(response) => {
            let status = response.status().as_u16();
            let body: Value = response.json().await.unwrap_or(Value::Null);
            classify_poll(status, &body)
        }
    }
}

/// Request a code, hand it to `on_code` (the UI shows the QR), then poll until the phone
/// approves, the code expires, the user declines or `cancel` is set. `sleep` waits one interval.
pub async fn run<S, F>(
    http: &reqwest::Client,
    hub: &str,
    cancel: &AtomicBool,
    sleep: S,
    on_code: impl FnOnce(&DeviceCode),
) -> Result<SessionGrant, LoginFailure>
where
    S: Fn(Duration) -> F,
    F: Future<Output = ()>,
{
    let code = request_code(http, hub).await?;
    on_code(&code);
    let mut schedule = PollSchedule::new(&code);
    loop {
        sleep(schedule.interval()).await;
        if cancel.load(Ordering::SeqCst) {
            return Err(LoginFailure::Cancelled);
        }
        if !schedule.wait_done() {
            return Err(LoginFailure::Expired);
        }
        match poll_once(http, hub, &code.device_code).await {
            PollOutcome::Pending => {}
            PollOutcome::SlowDown(named) => schedule.slow_down(named),
            PollOutcome::Expired => return Err(LoginFailure::Expired),
            PollOutcome::Denied => return Err(LoginFailure::Denied),
            PollOutcome::Granted(grant) => return Ok(*grant),
            PollOutcome::Failed(failure) => return Err(failure),
        }
    }
}

/// A QR code as a boolean module grid (no quiet zone; the view adds one).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QrMatrix {
    width: usize,
    dark: Vec<bool>,
}

impl QrMatrix {
    pub fn encode(text: &str) -> Option<Self> {
        let code = QrCode::new(text.as_bytes()).ok()?;
        let width = code.width();
        let dark = code
            .to_colors()
            .into_iter()
            .map(|color| color == Color::Dark)
            .collect();
        Some(Self { width, dark })
    }

    pub fn width(&self) -> usize {
        self.width
    }

    pub fn is_dark(&self, x: usize, y: usize) -> bool {
        x < self.width && y < self.width && self.dark[y * self.width + x]
    }

    /// Row `y` as `(start, length)` runs of dark modules, so a view draws few elements.
    pub fn dark_runs(&self, y: usize) -> Vec<(usize, usize)> {
        let mut runs = Vec::new();
        let mut x = 0;
        while x < self.width {
            if self.is_dark(x, y) {
                let start = x;
                while x < self.width && self.is_dark(x, y) {
                    x += 1;
                }
                runs.push((start, x - start));
            } else {
                x += 1;
            }
        }
        runs
    }
}
