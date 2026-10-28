//! The bridge-facing side of the tunnel client: [`TunnelSink`] (`EventSink`) and
//! [`TunnelCommands`] (`CommandSource`), plus the state they share with the connection task.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::tunnel::MAX_FRAME_BYTES;
use serde_json::{Value, json};
use sidecar::bridge::{CommandFailure, CommandSource, EventSink, IncomingCommand, SinkError};
use sidecar::store::Store;
use tokio::sync::{Notify, mpsc};

use protocol::turn_event::Envelope;

/// State shared by the sink and the connection task.
pub(crate) struct Shared {
    pub store: Arc<Store>,
    /// Serialises outbox writes with `restart_outbound_stream`, which reads the outbox and
    /// rewrites it: an enqueue in between would be lost.
    pub outbox_lock: Mutex<()>,
    /// Signalled when a frame was added to the outbox.
    pub wake: Notify,
    /// Highest outbox seq handed out; lets the connection skip a store read when nothing is new.
    pub last_seq: AtomicU64,
    /// Set when the client stopped for good; the sink then refuses output.
    pub closed: AtomicBool,
}

impl Shared {
    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            outbox_lock: Mutex::new(()),
            wake: Notify::new(),
            last_seq: AtomicU64::new(0),
            closed: AtomicBool::new(false),
        }
    }

    pub fn lock_outbox(&self) -> MutexGuard<'_, ()> {
        self.outbox_lock
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Writes one up frame body to the durable outbox (before it can be sent) and wakes the
    /// connection. `body` is the frame without `seq` and `event_id`. Returns its seq.
    pub fn enqueue(&self, body: Value) -> Result<u64, SinkError> {
        self.enqueue_all(vec![body]).map(|seqs| seqs[0])
    }

    /// Like [`Shared::enqueue`] for several bodies that must stay contiguous and in order.
    pub fn enqueue_all(&self, bodies: Vec<Value>) -> Result<Vec<u64>, SinkError> {
        if self.closed.load(Ordering::Acquire) {
            return Err(SinkError("the tunnel client stopped".to_owned()));
        }
        let mut prepared = Vec::with_capacity(bodies.len());
        for body in bodies {
            // The frame as it will go on the wire must fit (§2) and parse (§3).
            let event_id = new_event_id();
            let text = frame_text(1, &event_id, &body);
            if text.len() > MAX_FRAME_BYTES {
                return Err(SinkError(format!(
                    "frame of {} bytes is too large",
                    text.len()
                )));
            }
            protocol::tunnel::parse_frame(&text)
                .map_err(|e| SinkError(format!("not a valid up frame: {e}")))?;
            prepared.push((event_id, body));
        }
        let mut seqs = Vec::with_capacity(prepared.len());
        {
            let _guard = self.lock_outbox();
            for (event_id, body) in &prepared {
                let seq = self
                    .store
                    .enqueue_outbound(event_id, body)
                    .map_err(|e| SinkError(format!("outbox write failed: {e}")))?;
                self.last_seq.fetch_max(seq, Ordering::AcqRel);
                seqs.push(seq);
            }
        }
        self.wake.notify_one();
        Ok(seqs)
    }
}

/// The wire text of an up frame: `body` plus its `seq` and `event_id`.
pub(crate) fn frame_text(seq: u64, event_id: &str, body: &Value) -> String {
    let mut obj = body.as_object().cloned().unwrap_or_default();
    obj.insert("seq".to_owned(), json!(seq));
    obj.insert("event_id".to_owned(), json!(event_id));
    Value::Object(obj).to_string()
}

/// A fresh ULID with the `evt_` prefix. Assigned once, when the frame enters the outbox.
pub(crate) fn new_event_id() -> String {
    const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";
    let ms = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| u128::from(u64::try_from(d.as_millis()).unwrap_or(0)))
        .unwrap_or(0);
    let random = rand::random::<u128>() & ((1u128 << 80) - 1);
    let value = (ms << 80) | random;
    let mut out = String::from("evt_");
    for i in (0..26).rev() {
        out.push(char::from(ALPHABET[((value >> (5 * i)) & 31) as usize]));
    }
    out
}

/// The bridge's `EventSink` over the tunnel. Cloneable; every clone feeds the same outbox.
#[derive(Clone)]
pub struct TunnelSink {
    pub(crate) shared: Arc<Shared>,
}

impl TunnelSink {
    /// Frames still waiting for the hub's ack (sent or not).
    pub fn unacked(&self) -> usize {
        self.shared
            .store
            .outbox_state()
            .map(|state| state.unacked.len())
            .unwrap_or(0)
    }
}

impl EventSink for TunnelSink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        let mut bodies = Vec::with_capacity(envelopes.len());
        for envelope in &envelopes {
            let envelope = serde_json::to_value(envelope)
                .map_err(|e| SinkError(format!("unserialisable envelope: {e}")))?;
            bodies.push(json!({ "t": "event", "envelope": envelope }));
        }
        self.shared.enqueue_all(bodies).map(|_| ())
    }

    async fn command_result(&self, failure: CommandFailure) -> Result<(), SinkError> {
        let code = serde_json::to_value(failure.code)
            .map_err(|e| SinkError(format!("unserialisable code: {e}")))?;
        self.shared
            .enqueue(json!({
                "t": "command_result",
                "idempotency_key": failure.idempotency_key,
                "ok": false,
                "code": code,
                "message": failure.message,
            }))
            .map(|_| ())
    }
}

/// The bridge's `CommandSource` over the tunnel.
pub struct TunnelCommands {
    pub(crate) rx: mpsc::UnboundedReceiver<IncomingCommand>,
    pub(crate) processed: mpsc::UnboundedSender<u64>,
}

impl CommandSource for TunnelCommands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        self.rx.recv().await
    }

    async fn processed(&mut self, seq: u64) {
        // The client is gone when this fails; nothing is left to ack.
        let _ = self.processed.send(seq);
    }
}
