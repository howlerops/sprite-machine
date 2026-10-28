//! The phone sign-in link of `sm rc` (client-api.md §5.7 A, M2-003c).
//!
//! A machine a member approved (`sm login`, device-code pairing) asks the hub for a one-time
//! sign-in link (`POST /v1/devices/signin-links`, authenticated by the machine credential) and
//! prints it as a terminal QR code and as a URL: at start, and again on every `SIGUSR1` (unix),
//! because a link is single use and lives 5 minutes. The runner never sees the session the
//! page gets from the link.
//!
//! The link code is a bearer secret. It goes to the output sink the caller gave (the terminal)
//! and nowhere else: never `tracing`, never an error message, never a file. A machine the
//! admin API created (no approving user) is refused by the hub (`403`); that is not an error
//! worth a message, `sm rc` simply has no link to print.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

use crate::login::flow::{normalize_hub, qr_text};
use crate::login::{KeychainCredentials, Login};
use crate::tunnel::CredentialSource;

/// Where the link block is printed. The terminal in `sm`; tests collect it.
pub type Sink = Arc<dyn Fn(&str) + Send + Sync>;

const REQUEST_TIMEOUT: Duration = Duration::from_secs(15);

/// A sign-in link: the URL carries the secret code.
struct Link {
    url: String,
}

/// The block printed for a link: the QR code, then the URL.
fn render(link: &Link) -> String {
    let mut block =
        String::from("\nSign in from your phone (the link works once, for 5 minutes):\n\n");
    if let Some(qr) = qr_text(&link.url) {
        block.push_str(&qr);
        block.push_str("\n\n");
    }
    block.push_str(&format!("    {}\n\n", link.url));
    block
}

/// The HTTP base of the hub: the stored login may name the tunnel's `ws(s)` form.
pub(super) fn http_base(hub: &str) -> Result<String> {
    let hub = hub.trim();
    let hub = if let Some(rest) = hub.strip_prefix("wss://") {
        format!("https://{rest}")
    } else if let Some(rest) = hub.strip_prefix("ws://") {
        format!("http://{rest}")
    } else {
        hub.to_owned()
    };
    normalize_hub(&hub)
}

/// Why no link: the hub said no (an admin-created machine, a revoked one), or the call failed.
/// The messages carry no URL, body or credential.
async fn request(http: &reqwest::Client, hub: &str, credential: &str) -> Result<Link> {
    let base = http_base(hub)?;
    let response = http
        .post(format!("{base}/v1/devices/signin-links"))
        .bearer_auth(credential)
        .json(&json!({}))
        .send()
        .await
        .map_err(|error| anyhow!("the hub did not answer ({})", error.without_url()))?;
    let status = response.status();
    if status.as_u16() == 403 {
        bail!("this machine was not paired by a member, so the hub offers no sign-in link");
    }
    if !status.is_success() {
        bail!("the hub answered {status}");
    }
    let body: Value = response
        .json()
        .await
        .context("the hub's answer was not JSON")?;
    let url = body
        .get("url")
        .and_then(Value::as_str)
        .filter(|url| {
            (url.starts_with("https://") || url.starts_with("http://"))
                && !url.chars().any(char::is_control)
        })
        .ok_or_else(|| anyhow!("the hub's answer has no usable url"))?;
    Ok(Link {
        url: url.to_owned(),
    })
}

/// Gets a link and prints it to `sink`; on a failure says why (without secrets) to `tracing`.
async fn announce(
    http: &reqwest::Client,
    credentials: &KeychainCredentials,
    hub: &str,
    sink: &Sink,
) {
    let credential = match credentials.credential().await {
        Ok(credential) => credential,
        Err(error) => {
            tracing::warn!("no sign-in link: {error}");
            return;
        }
    };
    match request(http, hub, &credential).await {
        Ok(link) => sink(&render(&link)),
        Err(error) => tracing::info!("no sign-in link: {error}"),
    }
}

/// Prints a link now and on every `SIGUSR1` until the returned task is aborted.
pub(super) fn spawn(sink: Sink, login: Login, credentials: KeychainCredentials) -> JoinHandle<()> {
    #[cfg(unix)]
    let mut usr1 =
        tokio::signal::unix::signal(tokio::signal::unix::SignalKind::user_defined1()).ok();
    tokio::spawn(async move {
        let http = match reqwest::Client::builder().timeout(REQUEST_TIMEOUT).build() {
            Ok(http) => http,
            Err(error) => {
                tracing::warn!("no sign-in link: cannot build the HTTP client: {error}");
                return;
            }
        };
        announce(&http, &credentials, &login.hub, &sink).await;
        #[cfg(unix)]
        if let Some(usr1) = usr1.as_mut() {
            while usr1.recv().await.is_some() {
                announce(&http, &credentials, &login.hub, &sink).await;
            }
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_tunnel_form_of_the_hub_maps_to_http() {
        assert_eq!(
            http_base("wss://hub.example/").unwrap(),
            "https://hub.example"
        );
        assert_eq!(
            http_base("ws://127.0.0.1:8787").unwrap(),
            "http://127.0.0.1:8787"
        );
        assert_eq!(
            http_base("https://hub.example").unwrap(),
            "https://hub.example"
        );
        // The credential never crosses the network in clear.
        assert!(http_base("ws://hub.example").is_err());
        assert!(http_base("http://hub.example").is_err());
    }

    #[test]
    fn the_block_has_the_qr_and_the_url() {
        let block = render(&Link {
            url: "https://hub.example/pair?code=sli_abc".to_owned(),
        });
        assert!(block.contains("https://hub.example/pair?code=sli_abc"));
        assert!(
            block.contains('\u{2580}') || block.contains('\u{2584}') || block.contains('\u{2588}')
        );
    }
}
