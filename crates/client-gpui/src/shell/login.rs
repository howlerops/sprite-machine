//! The login screen's state machine ([`LoginModel`]) and the background driver that runs the two
//! sign-in flows ([`LoginDriver`]). No GPUI here: the view reads the model and forwards clicks.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{Receiver, Sender, channel};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use super::auth::{LoginFailure, SessionGrant};
use super::browser::Browser;
use super::i18n;
use super::pairing::{self, DeviceCode};
use super::pkce::{self, GithubSignIn};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LoginMethod {
    Pairing,
    GitHub,
}

/// Where the login screen is.
#[derive(Debug, Clone, PartialEq)]
pub enum LoginStage {
    /// Pick a method.
    Choose,
    /// Talking to the hub, nothing to show yet.
    Starting(LoginMethod),
    /// Showing the QR code and the typed code for the phone.
    Pairing {
        user_code: String,
        verification_uri: String,
        qr_target: String,
        expires_minutes: u64,
    },
    /// The system browser is open; waiting for the loopback redirect.
    Browser,
    Failed(LoginFailure),
    SignedIn(SessionGrant),
}

#[derive(Debug, Clone, PartialEq)]
pub enum LoginEvent {
    Started(LoginMethod),
    PairingCode(DeviceCode),
    BrowserOpened,
    Finished(Result<SessionGrant, LoginFailure>),
    /// The user pressed Cancel or Try again.
    Reset,
}

#[derive(Debug, Clone, PartialEq)]
pub struct LoginModel {
    stage: LoginStage,
}

impl Default for LoginModel {
    fn default() -> Self {
        Self::new()
    }
}

impl LoginModel {
    pub fn new() -> Self {
        Self {
            stage: LoginStage::Choose,
        }
    }

    pub fn stage(&self) -> &LoginStage {
        &self.stage
    }

    /// Is a flow running (so Cancel is offered instead of the method buttons)?
    pub fn is_busy(&self) -> bool {
        matches!(
            self.stage,
            LoginStage::Starting(_) | LoginStage::Pairing { .. } | LoginStage::Browser
        )
    }

    pub fn apply(&mut self, event: LoginEvent) {
        self.stage = match (&self.stage, event) {
            // A finished sign-in is final; late events from a cancelled flow are dropped.
            (LoginStage::SignedIn(_), _) => return,
            (_, LoginEvent::Reset) => LoginStage::Choose,
            // Cancel is always preceded by Reset; this is the cancelled flow reporting back and
            // must not disturb a flow started since.
            (_, LoginEvent::Finished(Err(LoginFailure::Cancelled))) => return,
            (LoginStage::Choose | LoginStage::Failed(_), LoginEvent::Started(method)) => {
                LoginStage::Starting(method)
            }
            (LoginStage::Starting(LoginMethod::Pairing), LoginEvent::PairingCode(code)) => {
                LoginStage::Pairing {
                    qr_target: code.qr_target().to_string(),
                    user_code: code.user_code,
                    verification_uri: code.verification_uri,
                    expires_minutes: code.expires_in.div_ceil(60),
                }
            }
            (LoginStage::Starting(LoginMethod::GitHub), LoginEvent::BrowserOpened) => {
                LoginStage::Browser
            }
            (stage, LoginEvent::Finished(result)) if is_flow(stage) => match result {
                Ok(grant) => LoginStage::SignedIn(grant),
                Err(failure) => LoginStage::Failed(failure),
            },
            _ => return,
        };
    }

    /// The localized status line (also the accessibility label of the status element).
    pub fn status_text(&self) -> String {
        match &self.stage {
            LoginStage::Choose => i18n::tr("login-subtitle"),
            LoginStage::Starting(_) => i18n::tr("login-starting"),
            LoginStage::Pairing {
                verification_uri,
                expires_minutes,
                ..
            } => format!(
                "{} {}",
                i18n::tr_args("login-pairing-instructions", &[("uri", verification_uri)]),
                i18n::tr_args(
                    "login-pairing-expires",
                    &[("minutes", &expires_minutes.to_string())]
                )
            ),
            LoginStage::Browser => i18n::tr("login-browser-waiting"),
            LoginStage::Failed(failure) => failure.message(),
            LoginStage::SignedIn(grant) => {
                i18n::tr_args("login-signed-in", &[("user", &grant.user_label())])
            }
        }
    }
}

fn is_flow(stage: &LoginStage) -> bool {
    matches!(
        stage,
        LoginStage::Starting(_) | LoginStage::Pairing { .. } | LoginStage::Browser
    )
}

/// Runs a sign-in on its own thread (with its own small tokio runtime, so views never touch
/// tokio) and hands the resulting [`LoginEvent`]s to whoever calls [`LoginDriver::drain`].
pub struct LoginDriver {
    hub: String,
    org_id: Option<String>,
    browser: Arc<dyn Browser>,
    sender: Sender<LoginEvent>,
    receiver: Mutex<Receiver<LoginEvent>>,
    cancel: Arc<AtomicBool>,
    oauth_timeout: Duration,
    sleep_divisor: u32,
}

impl LoginDriver {
    pub fn new(hub: impl Into<String>, browser: Arc<dyn Browser>) -> Self {
        let (sender, receiver) = channel();
        Self {
            hub: hub.into(),
            org_id: None,
            browser,
            sender,
            receiver: Mutex::new(receiver),
            cancel: Arc::new(AtomicBool::new(false)),
            oauth_timeout: Duration::from_secs(300),
            sleep_divisor: 1,
        }
    }

    pub fn set_hub(&mut self, hub: impl Into<String>) {
        self.hub = hub.into();
    }

    /// Tests: shorten the GitHub wait.
    pub fn with_oauth_timeout(mut self, timeout: Duration) -> Self {
        self.oauth_timeout = timeout;
        self
    }

    /// Tests: divide every polling sleep by `divisor` (a 1 s interval becomes `1 s / divisor`).
    pub fn with_sleep_divisor(mut self, divisor: u32) -> Self {
        self.sleep_divisor = divisor.max(1);
        self
    }

    /// Begin a flow. The model should get [`LoginEvent::Started`] from the caller.
    pub fn start(&self, method: LoginMethod) {
        self.cancel.store(false, Ordering::SeqCst);
        let sender = self.sender.clone();
        let cancel = self.cancel.clone();
        let hub = self.hub.clone();
        let browser = self.browser.clone();
        let settings = GithubSignIn {
            hub: self.hub.clone(),
            org_id: self.org_id.clone(),
            timeout: self.oauth_timeout,
        };
        let divisor = self.sleep_divisor;
        std::thread::spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = sender.send(LoginEvent::Finished(Err(LoginFailure::Network(
                        error.to_string(),
                    ))));
                    return;
                }
            };
            let http = reqwest::Client::new();
            let result = runtime.block_on(async {
                match method {
                    LoginMethod::Pairing => {
                        let announce = sender.clone();
                        let watch = cancel.clone();
                        pairing::run(
                            &http,
                            &hub,
                            &cancel,
                            move |duration| cancellable_sleep(duration / divisor, watch.clone()),
                            move |code| {
                                let _ = announce.send(LoginEvent::PairingCode(code.clone()));
                            },
                        )
                        .await
                    }
                    LoginMethod::GitHub => {
                        let announce = sender.clone();
                        pkce::sign_in(&settings, browser.as_ref(), &http, &cancel, move || {
                            let _ = announce.send(LoginEvent::BrowserOpened);
                        })
                        .await
                    }
                }
            });
            let _ = sender.send(LoginEvent::Finished(result));
        });
    }

    /// Stop the running flow (it ends with `Finished(Err(Cancelled))`).
    pub fn cancel(&self) {
        self.cancel.store(true, Ordering::SeqCst);
    }

    /// Events produced since the last call.
    pub fn drain(&self) -> Vec<LoginEvent> {
        let receiver = self.receiver.lock().expect("login receiver");
        receiver.try_iter().collect()
    }
}

/// Sleep `total`, waking every 50 ms to notice a cancel.
async fn cancellable_sleep(total: Duration, cancel: Arc<AtomicBool>) {
    let slice = Duration::from_millis(50);
    let mut left = total;
    while !left.is_zero() && !cancel.load(Ordering::SeqCst) {
        let step = left.min(slice);
        tokio::time::sleep(step).await;
        left -= step;
    }
}
