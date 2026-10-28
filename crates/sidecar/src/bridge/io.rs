//! The two seams between the bridge and the tunnel client: where TurnEvents go
//! ([`EventSink`]) and where hub commands come from ([`CommandSource`]).
//!
//! The tunnel client (M2B-004) implements both over the WebSocket. Tests use the in-memory
//! implementations [`MemorySink`] and [`MemoryCommands`].

use std::future::Future;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::tunnel::{Command, CommandErrorCode};
use protocol::turn_event::Envelope;
use tokio::sync::mpsc;

/// The sink cannot take more output (the tunnel client stopped). The bridge stops too.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("event sink closed: {0}")]
pub struct SinkError(pub String);

/// A command the runner could not apply (tunnel.md §3.6). Success is visible as TurnEvents, so
/// only failures are reported.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CommandFailure {
    /// The key of the command frame that failed.
    pub idempotency_key: String,
    /// `capacity`, `unknown_thread`, `unknown_interrupt`, `unsupported`, `invalid`, ...
    pub code: CommandErrorCode,
    /// Human-readable detail.
    pub message: String,
}

/// Where a command came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Origin {
    /// Sent by the hub (a down-stream `command` frame). The hub already emitted the
    /// `UserMessage` of a prompt (client-api.md §6).
    Hub,
    /// Typed on the runner itself. The hub never saw it, so the bridge emits its `UserMessage`.
    Local,
}

/// One down-stream `command` frame, or a command typed locally.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IncomingCommand {
    /// The frame's down-stream seq (tunnel.md §3.7).
    pub seq: u64,
    /// The key the client sent with its HTTP request. The bridge applies each key once.
    pub idempotency_key: String,
    pub command: Command,
    /// Who sent the command. Only [`Origin::Local`] prompts get a `UserMessage` from the bridge.
    pub origin: Origin,
}

/// Where the bridge sends what it produces.
pub trait EventSink: Send + Sync + 'static {
    /// Takes TurnEvent envelopes in production order. Every envelope has `seq: 0`; the Thread
    /// DO numbers them. One call carries the events of one step, in order.
    fn send(&self, envelopes: Vec<Envelope>) -> impl Future<Output = Result<(), SinkError>> + Send;

    /// Reports a command that could not be applied (`command_result{ok: false}`).
    fn command_result(
        &self,
        failure: CommandFailure,
    ) -> impl Future<Output = Result<(), SinkError>> + Send;
}

/// Where hub commands come from.
pub trait CommandSource: Send + 'static {
    /// The next command, or `None` when the tunnel client shut down. Must be cancel safe.
    fn next(&mut self) -> impl Future<Output = Option<IncomingCommand>> + Send;

    /// The bridge has processed the frame (its effect applied or rejected, or the key was a
    /// duplicate). The source commits it now (`Store::commit_down_frame`: `last_down_seq` and
    /// `key`) and acks `seq`, not earlier. The bridge never commits down frames itself.
    fn processed(&mut self, seq: u64, idempotency_key: &str) -> impl Future<Output = ()> + Send {
        let _ = (seq, idempotency_key);
        async {}
    }
}

// ---- in-memory implementations -------------------------------------------------------------

/// What a [`MemorySink`] received.
#[derive(Debug, Clone, PartialEq)]
pub enum SinkItem {
    /// One `send` call.
    Events(Vec<Envelope>),
    /// One `command_result`.
    Failure(CommandFailure),
}

/// An [`EventSink`] that forwards everything to a channel.
#[derive(Clone)]
pub struct MemorySink {
    tx: mpsc::UnboundedSender<SinkItem>,
}

impl MemorySink {
    /// A sink and the receiver of what it gets.
    pub fn new() -> (Self, mpsc::UnboundedReceiver<SinkItem>) {
        let (tx, rx) = mpsc::unbounded_channel();
        (Self { tx }, rx)
    }
}

impl EventSink for MemorySink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        self.tx
            .send(SinkItem::Events(envelopes))
            .map_err(|_| SinkError("receiver dropped".to_owned()))
    }

    async fn command_result(&self, failure: CommandFailure) -> Result<(), SinkError> {
        self.tx
            .send(SinkItem::Failure(failure))
            .map_err(|_| SinkError("receiver dropped".to_owned()))
    }
}

/// A [`CommandSource`] fed through a [`CommandFeed`]. It records the seqs the bridge reports as
/// processed.
pub struct MemoryCommands {
    rx: mpsc::UnboundedReceiver<IncomingCommand>,
    processed: Arc<Mutex<Vec<(u64, String)>>>,
}

/// The sending side of a [`MemoryCommands`]. Dropping every clone ends the command stream.
#[derive(Clone)]
pub struct CommandFeed {
    tx: mpsc::UnboundedSender<IncomingCommand>,
    next_seq: Arc<Mutex<u64>>,
    processed: Arc<Mutex<Vec<(u64, String)>>>,
}

impl MemoryCommands {
    /// A command source and its feed.
    pub fn new() -> (Self, CommandFeed) {
        let (tx, rx) = mpsc::unbounded_channel();
        let processed = Arc::new(Mutex::new(Vec::new()));
        (
            Self {
                rx,
                processed: Arc::clone(&processed),
            },
            CommandFeed {
                tx,
                next_seq: Arc::new(Mutex::new(0)),
                processed,
            },
        )
    }
}

fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}

impl CommandFeed {
    /// Sends `command` as the next frame with a fresh idempotency key; returns the key.
    pub fn send(&self, command: Command) -> String {
        let seq = self.next_seq();
        let key = format!("idk_{seq}");
        self.send_frame(seq, &key, command);
        key
    }

    /// Sends `command` as a prompt typed locally (origin [`Origin::Local`]); returns the key.
    pub fn send_local(&self, command: Command) -> String {
        let seq = self.next_seq();
        let key = format!("idk_local_{seq}");
        self.send_origin(seq, &key, command, Origin::Local);
        key
    }

    /// Sends `command` as the next frame with the given key (a client or hub retry).
    pub fn send_with_key(&self, key: &str, command: Command) {
        let seq = self.next_seq();
        self.send_frame(seq, key, command);
    }

    /// Seqs the bridge has reported as processed, in order.
    pub fn processed(&self) -> Vec<u64> {
        lock(&self.processed).iter().map(|(seq, _)| *seq).collect()
    }

    /// `(seq, idempotency key)` the bridge has reported as processed, in order.
    pub fn processed_keys(&self) -> Vec<(u64, String)> {
        lock(&self.processed).clone()
    }

    fn next_seq(&self) -> u64 {
        let mut next = lock(&self.next_seq);
        *next += 1;
        *next
    }

    fn send_frame(&self, seq: u64, key: &str, command: Command) {
        self.send_origin(seq, key, command, Origin::Hub);
    }

    fn send_origin(&self, seq: u64, key: &str, command: Command, origin: Origin) {
        // A closed bridge means the test is over; nobody reads the command.
        let _ = self.tx.send(IncomingCommand {
            seq,
            idempotency_key: key.to_owned(),
            command,
            origin,
        });
    }
}

impl CommandSource for MemoryCommands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        self.rx.recv().await
    }

    async fn processed(&mut self, seq: u64, idempotency_key: &str) {
        lock(&self.processed).push((seq, idempotency_key.to_owned()));
    }
}
