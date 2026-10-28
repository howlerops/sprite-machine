// TRACER STUB: replaced by M2B-004 (runner tunnel client: durable outbox, reconnect with backoff, resume tokens, auth)
//! The tracer bullet's tunnel client: one outbound WebSocket to the hub
//! (`/v1/machines/{machine_id}/tunnel`, subprotocol `sm.tunnel.v1`) speaking the MVP frame
//! subset of tunnel.md §7 on the pure `protocol::tunnel` state machines.
//!
//! Up: every [`Envelope`] from the bridge becomes an `event` frame (`seq` from [`Sender`],
//! a fresh `event_id`). Down: `command` frames are checked by [`Receiver`], deduped by
//! `idempotency_key`, handed to the bridge and acked at once. A `{"t":"ping"}` goes out every
//! `heartbeat_ms` from `welcome`.
//!
//! Auth (M2B-002): the hub's tunnel needs a machine credential (`Authorization: Bearer smm_...`).
//! The tracer has no pairing yet (M2B-005), so it asks the hub's DEV_FIXTURES route
//! `POST /v1/dev/machines/{id}/credential` (`Bearer dev`) for one before it connects. Inbound
//! frames are parsed with the strict `parse_frame`.
//!
//! Stub limits: in-memory outbox, no reconnect (a lost socket ends the run with an error), no
//! resume token persistence, a dev credential instead of pairing, no `command_result`.

use std::future::Future;
use std::time::Duration;

use anyhow::{Context, Result, anyhow, bail};
use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::{
    Capabilities, CloseCode, Command, DedupeWindow, Disposition, Frame, PROTO_VERSION, Receiver,
    SUBPROTOCOL, Sender, Verdict, parse_frame,
};
use protocol::turn_event::Envelope;
use serde_json::Value;
use tokio::sync::mpsc;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};

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

/// One up-stream body before it gets its frame `seq`.
#[derive(Debug, Clone)]
struct UpEvent {
    event_id: String,
    envelope: Value,
}

/// Connects, says `hello`, and pumps frames until `shutdown` resolves or the bridge closes
/// `events` (then it waits for the hub to ack everything and closes with `normal`).
pub async fn run(
    config: TunnelConfig,
    mut events: mpsc::Receiver<Envelope>,
    commands: mpsc::Sender<Command>,
    shutdown: impl Future<Output = ()>,
) -> Result<()> {
    let url = format!(
        "{}/v1/machines/{}/tunnel",
        config.hub_ws_base.trim_end_matches('/'),
        config.machine_id
    );
    let credential = fetch_dev_credential(&config).await?;
    let mut request = url
        .as_str()
        .into_client_request()
        .with_context(|| format!("tunnel url {url}"))?;
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    // The credential only ever travels in this header (tunnel.md §2).
    let mut bearer = HeaderValue::from_str(&format!("Bearer {credential}"))
        .context("machine credential is not a header value")?;
    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let (socket, _) = tokio_tungstenite::connect_async(request)
        .await
        .with_context(|| format!("connect tunnel {url}"))?;
    let (mut sink, mut stream) = socket.split();

    let mut up: Sender<UpEvent> = Sender::new(256);
    let mut down = Receiver::fresh();
    let mut keys = DedupeWindow::idempotency_keys();
    let id_seed = format!("{:x}", now_ms());
    let mut next_event_id: u64 = 0;

    let hello = Frame::Hello {
        proto: PROTO_VERSION,
        machine_id: config.machine_id.clone(),
        runner_version: config.runner_version.clone(),
        resume_token: None,
        last_down_seq: down.last(),
        capabilities: Capabilities {
            host_mode: "local".to_owned(),
            harnesses: vec!["acp-mock".to_owned()],
            max_sessions: 1,
            spawn: vec!["same_dir".to_owned()],
            features: Vec::new(),
        },
    };
    sink.send(text(&hello)?).await.context("send hello")?;

    let welcome = tokio::time::timeout(WELCOME_TIMEOUT, next_frame(&mut stream))
        .await
        .map_err(|_| anyhow!("no welcome within {WELCOME_TIMEOUT:?}"))??;
    let (resumed, last_up_seq, heartbeat_ms) = match welcome {
        Frame::Welcome {
            resumed,
            last_up_seq,
            heartbeat_ms,
            ..
        } => (resumed, last_up_seq, heartbeat_ms),
        Frame::Close { code, message } => bail!("hub closed the tunnel: {code:?} {message}"),
        other => bail!("expected welcome, got {other:?}"),
    };
    up.on_welcome(resumed, last_up_seq)
        .map_err(|e| anyhow!("welcome: {e}"))?;
    let mut heartbeat = tokio::time::interval(Duration::from_millis(heartbeat_ms.max(1000)));
    heartbeat.tick().await;

    tokio::pin!(shutdown);
    let mut events_open = true;
    loop {
        // Put every frame the window allows on the wire.
        while let Some((seq, body)) = up.next_to_send() {
            let frame = Frame::Event {
                seq,
                event_id: body.event_id,
                envelope: body.envelope,
            };
            sink.send(text(&frame)?).await.context("send event")?;
        }
        if !events_open && up.is_empty() {
            break;
        }
        tokio::select! {
            () = &mut shutdown => break,
            envelope = events.recv(), if events_open => match envelope {
                Some(envelope) => {
                    next_event_id += 1;
                    up.enqueue(UpEvent {
                        event_id: format!("evt_{id_seed}_{next_event_id}"),
                        envelope: serde_json::to_value(&envelope)?,
                    });
                }
                None => events_open = false,
            },
            frame = next_frame(&mut stream) => match frame? {
                Frame::Command { seq, idempotency_key, command, .. } => {
                    match down.on_frame(seq) {
                        Err(error) => {
                            let close = Frame::Close {
                                code: CloseCode::ProtocolError,
                                message: error.to_string(),
                            };
                            let _ = sink.send(text(&close)?).await;
                            bail!("down stream: {error}");
                        }
                        Ok(Verdict::Duplicate { .. }) => {}
                        Ok(Verdict::Process) => {
                            if keys.observe(&idempotency_key, now_ms()) == Disposition::New {
                                commands
                                    .send(command)
                                    .await
                                    .map_err(|_| anyhow!("the bridge stopped taking commands"))?;
                            }
                        }
                    }
                    // Ack every command at once (duplicates re-ack `last`).
                    let seq = down.take_ack().unwrap_or(down.last());
                    sink.send(text(&Frame::Ack { seq })?).await.context("send ack")?;
                }
                Frame::Ack { seq } => up.on_ack(seq).map_err(|e| anyhow!("ack: {e}"))?,
                Frame::Nack { seq, code, message } => {
                    tracing::warn!(seq, ?code, %message, "hub rejected an event");
                    up.on_nack(seq).map_err(|e| anyhow!("nack: {e}"))?;
                }
                Frame::Ping => sink.send(text(&Frame::Pong)?).await.context("send pong")?,
                Frame::Pong => {}
                Frame::Close { code, message } => bail!("hub closed the tunnel: {code:?} {message}"),
                other => tracing::warn!(?other, "unexpected frame from the hub"),
            },
            _ = heartbeat.tick() => {
                sink.send(text(&Frame::Ping)?).await.context("send ping")?;
            }
        }
    }
    let close = Frame::Close {
        code: CloseCode::Normal,
        message: "runner shutting down".to_owned(),
    };
    let _ = sink.send(text(&close)?).await;
    let _ = sink.close().await;
    Ok(())
}

/// Asks the hub's DEV_FIXTURES route for a credential for this machine (replacing any earlier
/// one). TRACER STUB: the real runner gets its credential from pairing (M2B-005).
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

/// Reads the next tunnel frame, skipping WebSocket control messages.
async fn next_frame<S>(stream: &mut S) -> Result<Frame>
where
    S: futures_util::Stream<Item = Result<Message, tokio_tungstenite::tungstenite::Error>> + Unpin,
{
    loop {
        let message = stream
            .next()
            .await
            .ok_or_else(|| anyhow!("tunnel closed by the hub"))?
            .context("tunnel read")?;
        match message {
            Message::Text(body) => {
                return parse_frame(body.as_str())
                    .with_context(|| format!("invalid frame from the hub: {}", body.as_str()));
            }
            Message::Close(reason) => bail!("tunnel closed by the hub: {reason:?}"),
            Message::Binary(_) => bail!("binary frame from the hub"),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

fn text(frame: &Frame) -> Result<Message> {
    Ok(Message::text(serde_json::to_string(frame)?))
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
        .unwrap_or(0)
}
