//! The connection task: connect, `hello`, `welcome`, pump frames, reconnect (tunnel.md §2-§4).

use std::collections::{HashSet, VecDeque};
use std::future::Future;
use std::pin::Pin;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use futures_util::stream::{SplitSink, SplitStream};
use futures_util::{SinkExt, StreamExt};
use protocol::tunnel::{
    CloseCode, Frame, NackCode, PROTO_VERSION, ParsedCommand, Receiver, SUBPROTOCOL, Sender,
    Verdict, parse_command_frame, parse_frame,
};
use protocol::turn_event::{Envelope, Event, RunError, RunErrorReason, format_timestamp};
use rustls::{ClientConfig, RootCertStore};
use serde_json::{Value, json};
use sidecar::bridge::IncomingCommand;
use sidecar::store::{OutboxState, Store, StoreError};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tokio::time::Instant;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::http::header::{AUTHORIZATION, SEC_WEBSOCKET_PROTOCOL};
use tokio_tungstenite::tungstenite::{self, Message};
use tokio_tungstenite::{Connector, MaybeTlsStream, WebSocketStream};

use super::backoff::{Backoff, Jitter, RandomJitter};
use super::io::{Shared, TunnelCommands, TunnelSink, frame_text};
use super::tls;
use super::{SystemWallClock, TunnelConfig, WallClock};

type Ws = WebSocketStream<MaybeTlsStream<TcpStream>>;
type WsSink = SplitSink<Ws, Message>;
type WsStream = SplitStream<Ws>;

/// Where the machine credential comes from. The client asks before every connect, so a
/// rotated credential is picked up on reconnect. The value only ever goes into the
/// `Authorization` header (tunnel.md §2) and is never logged.
pub trait CredentialSource: Send + Sync + 'static {
    fn credential(&self) -> impl Future<Output = Result<String>> + Send;

    /// The hub revoked the machine (`auth_revoked`): forget the credential.
    fn wipe(&self) {}
}

/// A credential held in memory.
pub struct StaticCredential(String);

impl StaticCredential {
    pub fn new(credential: impl Into<String>) -> Self {
        Self(credential.into())
    }
}

impl std::fmt::Debug for StaticCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("StaticCredential(..)")
    }
}

impl CredentialSource for StaticCredential {
    async fn credential(&self) -> Result<String> {
        Ok(self.0.clone())
    }
}

/// Why [`TunnelClient::run`] returned `Ok`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Exit {
    /// The shutdown future resolved; the client sent `close{normal}`.
    Shutdown,
    /// The hub closed with a code that must not be retried (tunnel.md §3.8): `normal`,
    /// `superseded`, `auth_invalid`, `auth_revoked` or `version_unsupported`.
    Closed(CloseCode),
}

/// The tunnel client. Build it with [`super::tunnel`].
pub struct TunnelClient<C> {
    config: TunnelConfig,
    shared: Arc<Shared>,
    credentials: C,
    wall: Arc<dyn WallClock>,
    /// TLS for `wss://` hubs: the webpki roots unless [`TunnelClient::with_tls_roots`] replaced them.
    tls: Arc<ClientConfig>,
    backoff: Backoff,
    commands: mpsc::UnboundedSender<IncomingCommand>,
    processed: mpsc::UnboundedReceiver<String>,
    /// Down commands of the current stream in seq order. A command is committed
    /// (`last_down_seq` + idempotency key) only when the bridge reports it processed, and only
    /// from the front, so the cursor never skips an unprocessed command.
    pending: VecDeque<PendingDown>,
    /// Keys fed to the bridge and not yet reported processed, across reconnects: a redelivery
    /// of one is not fed again.
    in_flight: HashSet<String>,
    up_since: Option<Instant>,
}

pub(crate) fn build<C: CredentialSource>(
    config: TunnelConfig,
    store: Arc<Store>,
    credentials: C,
) -> (TunnelClient<C>, TunnelSink, TunnelCommands) {
    let shared = Arc::new(Shared::new(store));
    let (command_tx, command_rx) = mpsc::unbounded_channel();
    let (processed_tx, processed_rx) = mpsc::unbounded_channel();
    let backoff = Backoff::new(
        config.timing.backoff_base,
        config.timing.backoff_cap,
        Box::new(RandomJitter),
    );
    let client = TunnelClient {
        config,
        shared: Arc::clone(&shared),
        credentials,
        wall: Arc::new(SystemWallClock),
        tls: tls::client_config(tls::webpki_roots()),
        backoff,
        commands: command_tx,
        processed: processed_rx,
        pending: VecDeque::new(),
        in_flight: HashSet::new(),
        up_since: None,
    };
    (
        client,
        TunnelSink { shared },
        TunnelCommands {
            rx: command_rx,
            processed: processed_tx,
        },
    )
}

/// A down command waiting to be committed.
struct PendingDown {
    seq: u64,
    key: String,
    /// The bridge finished it (or it needs no bridge).
    done: bool,
}

/// How one connection ended.
enum Ended {
    Shutdown,
    /// The hub (or the WebSocket close code) gave a reason.
    Close(CloseCode),
    /// Network loss, timeout or a protocol violation: reconnect with backoff.
    Failed(String),
    /// The wall clock jumped past the monotonic clock: the machine slept. Reconnect at once.
    WakeGap,
}

enum SessErr {
    Lost(String),
    Fatal(anyhow::Error),
}

impl From<StoreError> for SessErr {
    fn from(error: StoreError) -> Self {
        Self::Fatal(anyhow!(error).context("tunnel store"))
    }
}

impl From<serde_json::Error> for SessErr {
    fn from(error: serde_json::Error) -> Self {
        Self::Fatal(anyhow!(error).context("tunnel frame"))
    }
}

/// One thing the connection loop reacts to.
enum Wakeup {
    Shutdown,
    Outbox,
    Processed(String),
    Inbound(Inbound),
    Tick,
}

/// One message from the hub, parsed.
enum Inbound {
    Frame(Box<Frame>),
    /// A `command` frame with a `kind` this build does not know.
    Unsupported {
        seq: u64,
        key: String,
        kind: String,
    },
    /// The WebSocket close handshake, with its code if it had one.
    WsClose(Option<u16>),
    Lost(String),
    Invalid(String),
}

impl<C: CredentialSource> TunnelClient<C> {
    /// Replaces the wall clock (tests simulate an OS sleep with it).
    pub fn with_wall_clock(mut self, wall: Arc<dyn WallClock>) -> Self {
        self.wall = wall;
        self
    }

    /// Trusts `roots` for `wss://` hubs instead of the webpki roots. Production does not call
    /// this; tests use it to trust a generated CA.
    pub fn with_tls_roots(mut self, roots: RootCertStore) -> Self {
        self.tls = tls::client_config(roots);
        self
    }

    /// Replaces the backoff jitter source.
    pub fn with_jitter(mut self, jitter: Box<dyn Jitter>) -> Self {
        let timing = self.config.timing;
        self.backoff = Backoff::new(timing.backoff_base, timing.backoff_cap, jitter);
        self
    }

    /// Keeps the tunnel up until `shutdown` resolves (then it sends `close{normal}`) or the hub
    /// closes with a code that forbids reconnecting. Everything else reconnects with backoff.
    /// Errors: the store failed, or the hub refused the upgrade (HTTP 401 / 403 / 426).
    pub async fn run(mut self, shutdown: impl Future<Output = ()>) -> Result<Exit> {
        tokio::pin!(shutdown);
        let result = self.run_loop(shutdown.as_mut()).await;
        self.shared
            .closed
            .store(true, std::sync::atomic::Ordering::Release);
        result
    }

    async fn run_loop(&mut self, mut shutdown: Pin<&mut impl Future<Output = ()>>) -> Result<Exit> {
        loop {
            let ended = self.session(shutdown.as_mut()).await?;
            if let Some(since) = self.up_since.take()
                && since.elapsed() >= self.config.timing.stable_after
            {
                self.backoff.reset();
            }
            let delay = match ended {
                Ended::Shutdown => return Ok(Exit::Shutdown),
                Ended::Close(code) if !reconnects(code) => {
                    if code == CloseCode::AuthRevoked {
                        self.credentials.wipe();
                    }
                    return Ok(Exit::Closed(code));
                }
                Ended::Close(CloseCode::ServerRestart) => {
                    // No penalty: start at the minimum delay (§3.8).
                    self.backoff.reset();
                    self.backoff.next_delay()
                }
                Ended::WakeGap => {
                    self.backoff.reset();
                    Duration::ZERO
                }
                Ended::Close(code) => {
                    tracing::warn!(?code, "tunnel closed by the hub; reconnecting");
                    self.backoff.next_delay()
                }
                Ended::Failed(reason) => {
                    tracing::warn!(%reason, "tunnel lost; reconnecting");
                    self.backoff.next_delay()
                }
            };
            tokio::select! {
                () = tokio::time::sleep(delay) => {}
                () = shutdown.as_mut() => return Ok(Exit::Shutdown),
            }
        }
    }

    async fn session(&mut self, shutdown: Pin<&mut impl Future<Output = ()>>) -> Result<Ended> {
        let credential = match self.credentials.credential().await {
            Ok(credential) => credential,
            Err(error) => return Ok(Ended::Failed(format!("credential: {error:#}"))),
        };
        let timeout = self.config.timing.welcome_timeout;
        let connected =
            tokio::time::timeout(timeout, connect(&self.config, &credential, &self.tls)).await;
        drop(credential);
        let ws = match connected {
            Err(_) => return Ok(Ended::Failed("connect timed out".to_owned())),
            Ok(Err(ConnectError::Fatal(message))) => bail!(message),
            Ok(Err(ConnectError::Retry(message))) => return Ok(Ended::Failed(message)),
            Ok(Ok(ws)) => ws,
        };
        match self.connected(ws, shutdown).await {
            Ok(ended) => Ok(ended),
            Err(SessErr::Lost(reason)) => Ok(Ended::Failed(reason)),
            Err(SessErr::Fatal(error)) => Err(error),
        }
    }

    /// A down command in stream order. Nothing is committed here: the cursor and the key move
    /// when the bridge reports it processed, so a crash before that gets it redelivered.
    fn accept_command(
        &mut self,
        seq: u64,
        key: String,
        command: protocol::tunnel::Command,
    ) -> Result<(), SessErr> {
        // Redelivered after a resumed reconnect while still pending: already tracked.
        if self.pending.iter().any(|p| p.seq == seq && p.key == key) {
            return Ok(());
        }
        if self.shared.store.seen_key(&key)? {
            // Applied before (a renumbered stream replays it): ack, do not apply.
            self.pending.push_back(PendingDown {
                seq,
                key,
                done: true,
            });
        } else if self.in_flight.contains(&key) {
            // The bridge still has it from before the reconnect: wait for its report.
            self.pending.push_back(PendingDown {
                seq,
                key,
                done: false,
            });
        } else {
            self.in_flight.insert(key.clone());
            self.pending.push_back(PendingDown {
                seq,
                key: key.clone(),
                done: false,
            });
            let fed = self.commands.send(IncomingCommand {
                seq,
                idempotency_key: key,
                command,
            });
            if fed.is_err() {
                tracing::warn!(seq, "the bridge stopped taking commands");
            }
        }
        self.commit_done()
    }

    /// A command kind this build does not know: answer `unsupported` (§3.5), nothing to apply.
    fn accept_unsupported(&mut self, seq: u64, key: String, kind: &str) -> Result<(), SessErr> {
        if !self.pending.iter().any(|p| p.seq == seq && p.key == key) {
            if !self.shared.store.seen_key(&key)? {
                let result = self.shared.enqueue(json!({
                    "t": "command_result",
                    "idempotency_key": key,
                    "ok": false,
                    "code": "unsupported",
                    "message": format!("unsupported command kind `{kind}`"),
                }));
                if let Err(error) = result {
                    tracing::warn!(%error, "could not queue the unsupported answer");
                }
            }
            self.pending.push_back(PendingDown {
                seq,
                key,
                done: true,
            });
        }
        self.commit_done()
    }

    /// Commits the finished commands at the front of the queue, in order.
    fn commit_done(&mut self) -> Result<(), SessErr> {
        while self.pending.front().is_some_and(|p| p.done) {
            if let Some(entry) = self.pending.pop_front() {
                self.shared
                    .store
                    .commit_down_frame(entry.seq, Some(&entry.key))?;
            }
        }
        Ok(())
    }

    async fn connected(
        &mut self,
        ws: Ws,
        mut shutdown: Pin<&mut impl Future<Output = ()>>,
    ) -> Result<Ended, SessErr> {
        let store = Arc::clone(&self.shared.store);
        let (mut sink, mut stream) = ws.split();

        // `hello` first, nothing else until `welcome` (§3.1).
        let hello = Frame::Hello {
            proto: PROTO_VERSION,
            machine_id: self.config.machine_id.clone(),
            runner_version: self.config.runner_version.clone(),
            resume_token: store.resume_token()?,
            last_down_seq: store.last_down_seq()?,
            capabilities: self.config.capabilities.clone(),
        };
        send(&mut sink, serde_json::to_string(&hello)?).await?;

        let welcome = tokio::select! {
            () = shutdown.as_mut() => return Ok(goodbye(&mut sink).await),
            read = tokio::time::timeout(self.config.timing.welcome_timeout, read(&mut stream)) => {
                match read {
                    Err(_) => return Ok(Ended::Failed("no welcome in time".to_owned())),
                    Ok(inbound) => inbound,
                }
            }
        };
        let welcome = match welcome {
            Inbound::Frame(frame) => *frame,
            other => return Ok(terminal(other)),
        };
        let (resumed, resume_token, last_up_seq, heartbeat_ms, max_inflight) = match welcome {
            Frame::Welcome {
                resumed,
                resume_token,
                last_up_seq,
                heartbeat_ms,
                max_inflight,
                ..
            } => (
                resumed,
                resume_token,
                last_up_seq,
                heartbeat_ms,
                max_inflight,
            ),
            Frame::Close { code, .. } => return Ok(Ended::Close(code)),
            other => {
                return Err(
                    protocol_error(&mut sink, format!("expected welcome, got {other:?}")).await,
                );
            }
        };

        // The previous token died when `welcome` was sent (single use): persist the new one first.
        store.set_resume_token(Some(&resume_token))?;
        if !resumed {
            store.restart_outbound_stream()?;
            store.restart_down_stream()?;
        }
        let state = store.outbox_state()?;
        self.shared
            .last_seq
            .fetch_max(state.next_seq - 1, std::sync::atomic::Ordering::AcqRel);
        let mut sender = rebuild_sender(state);
        sender.set_max_inflight(u64::from(max_inflight));
        if resumed {
            // Drop what the hub processed, resend the rest (§4.2 rule 4).
            if let Err(error) = sender.on_welcome(true, last_up_seq) {
                // The hub's cursor disagrees with our outbox: forget the token so the
                // next connect starts a fresh stream.
                store.set_resume_token(None)?;
                return Err(SessErr::Lost(format!("welcome: {error}")));
            }
            store.ack_outbound(last_up_seq)?;
        } else {
            // New numbering: old seqs mean nothing. Keys still in `in_flight` stay, so a
            // redelivery of a command the bridge is still running is not applied twice.
            self.pending.clear();
        }
        let mut down = if resumed {
            Receiver::resume(store.last_down_seq()?)
        } else {
            Receiver::fresh()
        };
        let mut acked_sent = down.last();

        let period = Duration::from_millis(heartbeat_ms.max(10));
        let mut ticker = tokio::time::interval_at(Instant::now() + period, period);
        ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        let mut last_rx = Instant::now();
        let mut last_tick = Instant::now();
        let mut last_wall = self.wall.now_ms();
        self.up_since = Some(Instant::now());
        pump(&self.shared, &mut sender, &mut sink).await?;

        loop {
            // A duplicate down frame is re-acked even when nothing moved (§4.2 receiver rule 2).
            let mut reack = false;
            let wakeup = tokio::select! {
                () = shutdown.as_mut() => Wakeup::Shutdown,
                () = self.shared.wake.notified() => Wakeup::Outbox,
                Some(key) = self.processed.recv() => Wakeup::Processed(key),
                inbound = read(&mut stream) => Wakeup::Inbound(inbound),
                _ = ticker.tick() => Wakeup::Tick,
            };
            match wakeup {
                Wakeup::Shutdown => return Ok(goodbye(&mut sink).await),
                Wakeup::Outbox => {}
                Wakeup::Processed(key) => {
                    self.in_flight.remove(&key);
                    match self.pending.iter_mut().find(|p| p.key == key && !p.done) {
                        Some(entry) => entry.done = true,
                        // From a stream that was renumbered since: it was applied, so record
                        // the key, but it must not move this stream's cursor.
                        None => {
                            store.observe_idempotency_key(&key)?;
                        }
                    }
                    self.commit_done()?;
                }
                Wakeup::Tick => {
                    // OS sleep: the wall clock advanced much more than the monotonic one.
                    let wall = self.wall.now_ms();
                    let wall_gap = wall.saturating_sub(last_wall);
                    let mono_gap =
                        u64::try_from(last_tick.elapsed().as_millis()).unwrap_or(u64::MAX);
                    let heartbeat = u64::try_from(period.as_millis()).unwrap_or(u64::MAX);
                    last_wall = wall;
                    last_tick = Instant::now();
                    if wall_gap > mono_gap.saturating_add(heartbeat.saturating_mul(2)) {
                        return Ok(Ended::WakeGap);
                    }
                    if last_rx.elapsed() > period * 3 {
                        let close = Frame::Close {
                            code: CloseCode::HeartbeatTimeout,
                            message: "no traffic from the hub".to_owned(),
                        };
                        let _ = tokio::time::timeout(
                            Duration::from_secs(1),
                            send(&mut sink, serde_json::to_string(&close)?),
                        )
                        .await;
                        return Ok(Ended::Failed("heartbeat timeout".to_owned()));
                    }
                    send(&mut sink, serde_json::to_string(&Frame::Ping)?).await?;
                }
                Wakeup::Inbound(inbound) => {
                    last_rx = Instant::now();
                    match inbound {
                        Inbound::Frame(frame) => match *frame {
                            Frame::Ack { seq } => {
                                if let Err(error) = sender.on_ack(seq) {
                                    return Err(protocol_error(&mut sink, error.to_string()).await);
                                }
                                store.ack_outbound(seq)?;
                            }
                            Frame::Nack { seq, code, message } => {
                                let rejected = match sender.on_nack(seq) {
                                    Ok(rejected) => rejected,
                                    Err(error) => {
                                        return Err(
                                            protocol_error(&mut sink, error.to_string()).await
                                        );
                                    }
                                };
                                // A nack counts as an ack: drop the frame, fail the task.
                                store.ack_outbound(seq)?;
                                tracing::warn!(seq, ?code, %message, "hub rejected an up frame");
                                if let Some(body) = rejected {
                                    fail_task(
                                        &self.shared,
                                        self.wall.as_ref(),
                                        &body,
                                        code,
                                        &message,
                                    );
                                }
                            }
                            Frame::Command {
                                seq,
                                idempotency_key,
                                command,
                                ..
                            } => match down.on_frame(seq) {
                                Err(error) => {
                                    return Err(protocol_error(&mut sink, error.to_string()).await);
                                }
                                Ok(Verdict::Duplicate { .. }) => reack = true,
                                Ok(Verdict::Process) => {
                                    self.accept_command(seq, idempotency_key, command)?;
                                }
                            },
                            Frame::Ping => {
                                send(&mut sink, serde_json::to_string(&Frame::Pong)?).await?
                            }
                            Frame::Pong => {}
                            Frame::Close { code, .. } => return Ok(Ended::Close(code)),
                            other => {
                                return Err(protocol_error(
                                    &mut sink,
                                    format!("unexpected frame {other:?}"),
                                )
                                .await);
                            }
                        },
                        Inbound::Unsupported { seq, key, kind } => match down.on_frame(seq) {
                            Err(error) => {
                                return Err(protocol_error(&mut sink, error.to_string()).await);
                            }
                            Ok(Verdict::Duplicate { .. }) => reack = true,
                            Ok(Verdict::Process) => self.accept_unsupported(seq, key, &kind)?,
                        },
                        other => return Ok(terminal(other)),
                    }
                }
            }

            pump(&self.shared, &mut sender, &mut sink).await?;
            // Ack the down stream up to the last frame the bridge finished (cumulative).
            let ackable = self
                .pending
                .front()
                .map_or(down.last(), |first| first.seq - 1);
            if ackable > acked_sent || (reack && ackable > 0) {
                acked_sent = acked_sent.max(ackable);
                send(
                    &mut sink,
                    serde_json::to_string(&Frame::Ack { seq: ackable })?,
                )
                .await?;
            }
        }
    }
}

fn reconnects(code: CloseCode) -> bool {
    !matches!(
        code,
        CloseCode::Normal
            | CloseCode::Superseded
            | CloseCode::AuthInvalid
            | CloseCode::AuthRevoked
            | CloseCode::VersionUnsupported
    )
}

fn rebuild_sender(state: OutboxState) -> Sender<UpBody> {
    Sender::from_parts(
        state.acked_upto,
        state.next_seq,
        state
            .unacked
            .into_iter()
            .map(|row| {
                (
                    row.seq,
                    UpBody {
                        event_id: row.event_id,
                        body: row.body,
                    },
                )
            })
            .collect(),
    )
}

/// A frame body in the sender, without its `seq`.
#[derive(Clone)]
struct UpBody {
    event_id: String,
    body: Value,
}

/// Moves new outbox rows into the sender and puts every frame the window allows on the wire.
async fn pump(
    shared: &Shared,
    sender: &mut Sender<UpBody>,
    sink: &mut WsSink,
) -> Result<(), SessErr> {
    let last = shared.last_seq.load(std::sync::atomic::Ordering::Acquire);
    if last >= sender.next_seq() {
        for row in shared.store.outbound_since(sender.next_seq() - 1)? {
            {
                let seq = sender.enqueue(UpBody {
                    event_id: row.event_id,
                    body: row.body,
                });
                if seq != row.seq {
                    return Err(SessErr::Fatal(anyhow!(
                        "outbox seq {} does not match the sender's {seq}",
                        row.seq
                    )));
                }
            }
        }
    }
    while let Some((seq, body)) = sender.next_to_send() {
        send(sink, frame_text(seq, &body.event_id, &body.body)).await?;
    }
    Ok(())
}

/// The hub rejected `rejected`: the task it belonged to cannot continue, so queue
/// `RunError{error, code: "protocol"}` for it as a new frame (§3.7).
fn fail_task(
    shared: &Shared,
    wall: &dyn WallClock,
    rejected: &UpBody,
    code: NackCode,
    message: &str,
) {
    if rejected.body.get("t").and_then(Value::as_str) != Some("event") {
        return;
    }
    let Some(envelope) = rejected.body.get("envelope") else {
        return;
    };
    let text = |key: &str| envelope.get(key).and_then(Value::as_str).map(str::to_owned);
    let (Some(thread_id), Some(task_id)) = (text("thread_id"), text("task_id")) else {
        return;
    };
    // A rejected RunError must not start a loop.
    if envelope.pointer("/event/type").and_then(Value::as_str) == Some("RunError") {
        return;
    }
    let failure = Envelope::new(
        thread_id,
        Some(task_id),
        text("run_id"),
        format_timestamp(wall.now_ms()),
        Event::RunError(RunError {
            reason: RunErrorReason::Error,
            code: Some("protocol".to_owned()),
            message: format!("the hub rejected an event ({code:?}): {message}"),
        }),
    );
    match serde_json::to_value(&failure) {
        Ok(envelope) => {
            if let Err(error) = shared.enqueue(json!({ "t": "event", "envelope": envelope })) {
                tracing::warn!(%error, "could not queue RunError for a rejected event");
            }
        }
        Err(error) => tracing::warn!(%error, "could not serialise RunError"),
    }
}

async fn send(sink: &mut WsSink, text: String) -> Result<(), SessErr> {
    sink.send(Message::text(text))
        .await
        .map_err(|e| SessErr::Lost(format!("tunnel write: {e}")))
}

/// Tells the hub the runner violated the protocol (best effort) and returns the error.
async fn protocol_error(sink: &mut WsSink, message: String) -> SessErr {
    let close = Frame::Close {
        code: CloseCode::ProtocolError,
        message: message.clone(),
    };
    if let Ok(text) = serde_json::to_string(&close) {
        let _ = tokio::time::timeout(Duration::from_secs(1), send(sink, text)).await;
    }
    SessErr::Lost(format!("protocol error: {message}"))
}

/// A clean shutdown: `close{normal}`, then the WebSocket close.
async fn goodbye(sink: &mut WsSink) -> Ended {
    let close = Frame::Close {
        code: CloseCode::Normal,
        message: "runner shutting down".to_owned(),
    };
    if let Ok(text) = serde_json::to_string(&close) {
        let _ = tokio::time::timeout(Duration::from_secs(1), send(sink, text)).await;
    }
    let _ = tokio::time::timeout(Duration::from_secs(1), sink.close()).await;
    Ended::Shutdown
}

/// What a message that is not part of normal traffic means for the connection.
fn terminal(inbound: Inbound) -> Ended {
    match inbound {
        Inbound::Frame(frame) => match *frame {
            Frame::Close { code, .. } => Ended::Close(code),
            other => Ended::Failed(format!("unexpected frame {other:?}")),
        },
        Inbound::WsClose(Some(ws)) => CloseCode::ALL
            .into_iter()
            .find(|code| code.ws_code() == ws)
            .map_or_else(
                || Ended::Failed(format!("tunnel closed with code {ws}")),
                Ended::Close,
            ),
        Inbound::WsClose(None) => Ended::Failed("tunnel closed".to_owned()),
        Inbound::Lost(reason) | Inbound::Invalid(reason) => Ended::Failed(reason),
        Inbound::Unsupported { kind, .. } => Ended::Failed(format!("unexpected command {kind}")),
    }
}

/// Reads and parses the next tunnel message, skipping WebSocket control messages.
async fn read(stream: &mut WsStream) -> Inbound {
    loop {
        let message = match stream.next().await {
            None => return Inbound::Lost("tunnel closed without a close".to_owned()),
            Some(Err(error)) => return Inbound::Lost(format!("tunnel read: {error}")),
            Some(Ok(message)) => message,
        };
        match message {
            Message::Text(body) => return parse(body.as_str()),
            Message::Close(frame) => {
                return Inbound::WsClose(frame.map(|frame| u16::from(frame.code)));
            }
            Message::Binary(_) => return Inbound::Invalid("binary frame from the hub".to_owned()),
            Message::Ping(_) | Message::Pong(_) | Message::Frame(_) => {}
        }
    }
}

fn parse(text: &str) -> Inbound {
    if text.len() > protocol::tunnel::MAX_FRAME_BYTES {
        return Inbound::Invalid("frame over 1 MiB".to_owned());
    }
    let is_command = serde_json::from_str::<Value>(text)
        .ok()
        .and_then(|value| {
            value
                .get("t")
                .and_then(Value::as_str)
                .map(|t| t == "command")
        })
        .unwrap_or(false);
    if is_command {
        return match parse_command_frame(text) {
            Ok(ParsedCommand::Typed(frame)) => Inbound::Frame(frame),
            Ok(ParsedCommand::Unsupported {
                seq,
                idempotency_key,
                kind,
                ..
            }) => Inbound::Unsupported {
                seq,
                key: idempotency_key,
                kind,
            },
            Err(error) => Inbound::Invalid(format!("invalid command frame: {error}")),
        };
    }
    match parse_frame(text) {
        Ok(frame) => Inbound::Frame(Box::new(frame)),
        Err(error) => Inbound::Invalid(format!("invalid frame from the hub: {error}")),
    }
}

enum ConnectError {
    /// The hub refused the upgrade for a reason retrying cannot fix.
    Fatal(String),
    Retry(String),
}

async fn connect(
    config: &TunnelConfig,
    credential: &str,
    tls: &Arc<ClientConfig>,
) -> Result<Ws, ConnectError> {
    // A refused scheme or host fails here, before any socket is opened.
    let hub = tls::hub_url(&config.hub_ws_base).map_err(ConnectError::Fatal)?;
    let url = format!("{}/v1/machines/{}/tunnel", hub.url, config.machine_id);
    let mut request = url
        .as_str()
        .into_client_request()
        .map_err(|e| ConnectError::Fatal(format!("tunnel url {url}: {e}")))?;
    request.headers_mut().insert(
        SEC_WEBSOCKET_PROTOCOL,
        HeaderValue::from_static(SUBPROTOCOL),
    );
    // The credential only ever travels in this header (tunnel.md §2).
    let mut bearer = HeaderValue::from_str(&format!("Bearer {credential}"))
        .map_err(|_| ConnectError::Fatal("machine credential is not a header value".to_owned()))?;
    bearer.set_sensitive(true);
    request.headers_mut().insert(AUTHORIZATION, bearer);
    let connector = hub.tls.then(|| Connector::Rustls(Arc::clone(tls)));
    match tokio_tungstenite::connect_async_tls_with_config(request, None, false, connector).await {
        Ok((ws, _)) => Ok(ws),
        Err(tungstenite::Error::Http(response)) => {
            let status = response.status().as_u16();
            let message = format!("hub refused the tunnel upgrade: HTTP {status}");
            if matches!(status, 401 | 403 | 426) {
                Err(ConnectError::Fatal(message))
            } else {
                Err(ConnectError::Retry(message))
            }
        }
        Err(error) => Err(ConnectError::Retry(format!("connect {url}: {error}"))),
    }
}
