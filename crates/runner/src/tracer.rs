//! The tracer bullet's runner: the real tunnel client ([`crate::tunnel`]) wired to the
//! tracer bridge's channels.
//!
//! Up: every [`Envelope`] from the bridge goes through the client's `EventSink`, so it is
//! written to a (temporary) durable outbox before it is sent. Down: `command` frames are
//! deduped and acked by the client; the bridge's `mpsc` takes the commands.
//!
//! Auth (M2B-002): the hub's tunnel needs a machine credential (`Authorization: Bearer smm_...`).
//! The tracer has no pairing yet (M2B-005), so it asks the hub's DEV_FIXTURES route
//! `POST /v1/dev/machines/{id}/credential` (`Bearer dev`) for one before every connect.

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use protocol::tunnel::{Capabilities, Command};
use protocol::turn_event::Envelope;
use serde_json::Value;
use sidecar::bridge::{CommandSource, EventSink};
use sidecar::store::Store;
use tokio::sync::{mpsc, oneshot};

use crate::tunnel::{self, CredentialSource, Exit, Timing};

/// How long the runner waits for `welcome` after `hello` (tunnel.md §3.1).
pub const WELCOME_TIMEOUT: Duration = Duration::from_secs(10);

/// Tunnel client settings.
#[derive(Debug, Clone)]
pub struct TunnelConfig {
    /// `ws://host:port` or `wss://host` (no path).
    pub hub_ws_base: String,
    pub machine_id: String,
    pub runner_version: String,
}

/// Connects, says `hello`, and pumps frames until `shutdown` resolves or the bridge closes
/// `events` (then it waits for the hub to ack everything and closes with `normal`). A lost
/// socket reconnects with backoff and resumes.
pub async fn run(
    config: TunnelConfig,
    mut events: mpsc::Receiver<Envelope>,
    commands: mpsc::Sender<Command>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let state_dir = std::env::temp_dir().join(format!(
        "sm-tracer-{}-{}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let store = Arc::new(Store::open(&state_dir).context("open the tracer outbox")?);
    let (client, sink, mut source) = tunnel::tunnel(
        tunnel::TunnelConfig {
            hub_ws_base: config.hub_ws_base.clone(),
            machine_id: config.machine_id.clone(),
            runner_version: config.runner_version.clone(),
            capabilities: Capabilities {
                host_mode: "local".to_owned(),
                harnesses: vec!["acp-mock".to_owned()],
                max_sessions: 1,
                spawn: vec!["same_dir".to_owned()],
                features: Vec::new(),
            },
            timing: Timing {
                welcome_timeout: WELCOME_TIMEOUT,
                ..Timing::default()
            },
        },
        store,
        DevCredential { config },
    );

    // Bridge events -> outbox. When the bridge is done, wait until the hub acked everything.
    let (drained_tx, drained_rx) = oneshot::channel::<()>();
    let up = tokio::spawn({
        let sink = sink.clone();
        async move {
            while let Some(envelope) = events.recv().await {
                if sink.send(vec![envelope]).await.is_err() {
                    return;
                }
            }
            while sink.unacked() > 0 {
                tokio::time::sleep(Duration::from_millis(20)).await;
            }
            let _ = drained_tx.send(());
        }
    });
    // Hub commands -> bridge; the bridge's channel taking one counts as processed.
    let down = tokio::spawn(async move {
        while let Some(incoming) = source.next().await {
            if commands.send(incoming.command).await.is_err() {
                return;
            }
            source.processed(incoming.seq).await;
        }
    });

    let stop = async {
        tokio::select! {
            () = shutdown => {}
            Ok(()) = drained_rx => {}
        }
    };
    let exit = client.run(stop).await;
    up.abort();
    down.abort();
    let _ = std::fs::remove_dir_all(&state_dir);
    match exit? {
        Exit::Shutdown => Ok(()),
        Exit::Closed(code) => bail!("hub closed the tunnel: {code:?}"),
    }
}

/// The tracer's credential: a fresh one from the hub's DEV_FIXTURES route on every connect.
/// TRACER STUB: the real runner gets its credential from pairing (M2B-005).
struct DevCredential {
    config: TunnelConfig,
}

impl CredentialSource for DevCredential {
    async fn credential(&self) -> Result<String> {
        fetch_dev_credential(&self.config).await
    }
}

/// Asks the hub's DEV_FIXTURES route for a credential for this machine (replacing any earlier
/// one).
async fn fetch_dev_credential(config: &TunnelConfig) -> Result<String> {
    let http_base = config
        .hub_ws_base
        .trim_end_matches('/')
        .replacen("ws", "http", 1);
    let url = format!(
        "{http_base}/v1/dev/machines/{}/credential",
        config.machine_id
    );
    let response = reqwest::Client::new()
        .post(&url)
        .header("Authorization", "Bearer dev")
        .send()
        .await
        .with_context(|| format!("request a dev credential from {url}"))?;
    let status = response.status();
    if !status.is_success() {
        bail!("{url} answered HTTP {status} (is the hub running with DEV_FIXTURES=1?)");
    }
    let body: Value = response.json().await.context("dev credential answer")?;
    body.get("credential")
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| anyhow!("the dev credential answer has no `credential`"))
}
