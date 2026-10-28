//! The bridge-facing side of the tunnel client: [`TunnelSink`] (`EventSink`) and
//! [`TunnelCommands`] (`CommandSource`), plus the state they share with the connection task.

use std::collections::HashMap;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use std::time::Duration;

use protocol::tunnel::{Actor, AnswerStatus, ContentPart, MAX_FRAME_BYTES, Outcome, Workspace};
use serde_json::{Value, json};
use sidecar::bridge::{CommandFailure, CommandSource, EventSink, IncomingCommand, SinkError};
use sidecar::store::Store;
use tokio::sync::{Notify, mpsc, oneshot};

use protocol::turn_event::Envelope;

/// State shared by the sink and the connection task.
pub(crate) struct Shared {
    pub store: Arc<Store>,
    /// Signalled when a frame was added to the outbox.
    pub wake: Notify,
    /// Highest outbox seq handed out; lets the connection skip a store read when nothing is new.
    pub last_seq: AtomicU64,
    /// Set when the client stopped for good; the sink then refuses output.
    pub closed: AtomicBool,
    /// Threads started on this runner (tunnel.md §3.4). `false`: marked, `thread_open` not yet
    /// queued; `true`: opened. A thread not listed here was dispatched by the hub and needs no
    /// `thread_open`. Held across the check and the enqueue, so `thread_open` is always the
    /// first frame of its thread in the outbox.
    pub local_threads: Mutex<HashMap<String, bool>>,
    /// Interrupt answers waiting for the hub's verdict, by `answer_id` (tunnel.md §3.6a).
    pub answers: Mutex<HashMap<String, oneshot::Sender<Result<AnswerReply, String>>>>,
}

/// The hub's verdict on one `interrupt_answer` (tunnel.md §3.6a).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AnswerReply {
    pub status: AnswerStatus,
    /// Who won: the local user for `accepted`, the other answerer for `already_resolved`.
    pub resolved_by: Option<Actor>,
    pub message: String,
}

impl Shared {
    fn lock_local_threads(&self) -> std::sync::MutexGuard<'_, HashMap<String, bool>> {
        self.local_threads
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    pub fn new(store: Arc<Store>) -> Self {
        Self {
            store,
            wake: Notify::new(),
            last_seq: AtomicU64::new(0),
            closed: AtomicBool::new(false),
            local_threads: Mutex::new(HashMap::new()),
            answers: Mutex::new(HashMap::new()),
        }
    }

    fn lock_answers(
        &self,
    ) -> std::sync::MutexGuard<'_, HashMap<String, oneshot::Sender<Result<AnswerReply, String>>>>
    {
        self.answers
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// Hands the hub's verdict (or its refusal of the frame) to whoever waits for `answer_id`.
    pub fn finish_answer(&self, answer_id: &str, verdict: Result<AnswerReply, String>) {
        if let Some(waiter) = self.lock_answers().remove(answer_id) {
            let _ = waiter.send(verdict);
        }
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
        for (event_id, body) in &prepared {
            let seq = self
                .store
                .enqueue_outbound(event_id, body)
                .map_err(|e| SinkError(format!("outbox write failed: {e}")))?;
            self.last_seq.fetch_max(seq, Ordering::AcqRel);
            seqs.push(seq);
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

    /// Declares `thread_id` as started on this runner (a local session). Until
    /// [`TunnelSink::thread_open`] is called for it, events for it are refused.
    pub fn mark_local_thread(&self, thread_id: &str) {
        let mut local = self.shared.lock_local_threads();
        local.entry(thread_id.to_owned()).or_insert(false);
    }

    /// Queues `thread_open` (tunnel.md §3.4) for a thread started on this runner. Call it
    /// before the thread's first event. At most once per thread id: a second call is an error.
    /// The outbox sends frames in seq order, so the hub sees it first, also when the frames are
    /// resent after a reconnect. Returns the outbox seq.
    pub fn thread_open(
        &self,
        thread_id: &str,
        title: &str,
        agent: &str,
        workspace: &Workspace,
    ) -> Result<u64, SinkError> {
        let workspace = serde_json::to_value(workspace)
            .map_err(|e| SinkError(format!("unserialisable workspace: {e}")))?;
        let mut local = self.shared.lock_local_threads();
        if local.get(thread_id) == Some(&true) {
            return Err(SinkError(format!("thread {thread_id} is already open")));
        }
        let seq = self.shared.enqueue(json!({
            "t": "thread_open",
            "thread_id": thread_id,
            "title": title,
            "agent": agent,
            "workspace": workspace,
        }))?;
        local.insert(thread_id.to_owned(), true);
        Ok(seq)
    }

    /// Sends an interrupt answer typed on this runner up and waits for the hub's verdict
    /// (tunnel.md §3.6a). The frame is in the durable outbox first, so it survives a dead
    /// socket; `Err` means no verdict came within `wait` (or the hub refused the frame), and
    /// the answer may still be applied by the hub later.
    pub async fn interrupt_answer(
        &self,
        thread_id: &str,
        interrupt_id: &str,
        outcome: &Outcome,
        wait: Duration,
    ) -> Result<AnswerReply, String> {
        let answer_id = new_event_id().replacen("evt_", "ans_", 1);
        let (tx, rx) = oneshot::channel();
        self.shared.lock_answers().insert(answer_id.clone(), tx);
        let queued = self.shared.enqueue(json!({
            "t": "interrupt_answer",
            "thread_id": thread_id,
            "interrupt_id": interrupt_id,
            "answer_id": answer_id,
            "outcome": outcome,
        }));
        if let Err(error) = queued {
            self.shared.lock_answers().remove(&answer_id);
            return Err(error.to_string());
        }
        let verdict = tokio::time::timeout(wait, rx).await;
        self.shared.lock_answers().remove(&answer_id);
        match verdict {
            Ok(Ok(verdict)) => verdict,
            Ok(Err(_)) => Err("the tunnel stopped before the hub answered".to_owned()),
            Err(_) => Err("the hub has not answered yet".to_owned()),
        }
    }

    /// Sends a prompt typed on this runner up (`prompt_submit`, tunnel.md §3.6b). The frame is in
    /// the durable outbox first, so it survives a dead socket and goes out once, in order with
    /// the events and answers before it. The hub queues it like a phone's prompt and sends it
    /// back as a `prompt` command; nothing is applied here. Returns the outbox seq.
    pub fn prompt_submit(
        &self,
        thread_id: &str,
        task_id: &str,
        message_id: &str,
        parts: &[ContentPart],
    ) -> Result<u64, SinkError> {
        let parts = serde_json::to_value(parts)
            .map_err(|e| SinkError(format!("unserialisable parts: {e}")))?;
        self.shared.enqueue(json!({
            "t": "prompt_submit",
            "thread_id": thread_id,
            "task_id": task_id,
            "message_id": message_id,
            "parts": parts,
        }))
    }

    /// Queues event bodies, refusing the whole batch if an envelope belongs to a local thread
    /// that was never opened.
    fn send_events(&self, envelopes: &[Envelope]) -> Result<(), SinkError> {
        let mut bodies = Vec::with_capacity(envelopes.len());
        for envelope in envelopes {
            let envelope = serde_json::to_value(envelope)
                .map_err(|e| SinkError(format!("unserialisable envelope: {e}")))?;
            bodies.push(json!({ "t": "event", "envelope": envelope }));
        }
        let local = self.shared.lock_local_threads();
        for envelope in envelopes {
            if local.get(&envelope.thread_id) == Some(&false) {
                return Err(SinkError(format!(
                    "thread {} was started on this runner but not opened (thread_open first)",
                    envelope.thread_id
                )));
            }
        }
        self.shared.enqueue_all(bodies).map(|_| ())
    }
}

impl EventSink for TunnelSink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        self.send_events(&envelopes)
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
    /// Idempotency keys of processed commands (the key, not the seq, so a late report for an
    /// old stream's command cannot be mistaken for a new command with the same seq).
    pub(crate) processed: mpsc::UnboundedSender<String>,
}

impl CommandSource for TunnelCommands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        self.rx.recv().await
    }

    async fn processed(&mut self, _seq: u64, idempotency_key: &str) {
        // The client is gone when this fails; nothing is left to ack.
        let _ = self.processed.send(idempotency_key.to_owned());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sink() -> (TunnelSink, tempfile::TempDir) {
        let dir = tempfile::TempDir::new().expect("state dir");
        let store = Arc::new(Store::open(dir.path()).expect("store"));
        (
            TunnelSink {
                shared: Arc::new(Shared::new(store)),
            },
            dir,
        )
    }

    fn allow() -> Outcome {
        Outcome::Selected {
            option_id: "allow".to_owned(),
        }
    }

    /// The answer is a durable `interrupt_answer` up frame, and the caller gets the hub's verdict
    /// for exactly that `answer_id`.
    #[tokio::test]
    async fn an_interrupt_answer_is_queued_up_and_resolves_with_the_hubs_verdict() {
        let (sink, _dir) = sink();
        let waiting = {
            let sink = sink.clone();
            tokio::spawn(async move {
                sink.interrupt_answer("th_1", "int_1", &allow(), Duration::from_secs(5))
                    .await
            })
        };
        let body = loop {
            let rows = sink.shared.store.outbound_since(0).expect("outbox");
            if let Some(row) = rows.into_iter().next() {
                break row.body;
            }
            tokio::task::yield_now().await;
        };
        assert_eq!(body["t"], "interrupt_answer");
        assert_eq!(body["thread_id"], "th_1");
        assert_eq!(body["interrupt_id"], "int_1");
        assert_eq!(body["outcome"]["option_id"], "allow");
        let answer_id = body["answer_id"].as_str().expect("answer_id").to_owned();
        assert!(answer_id.starts_with("ans_"), "{answer_id}");

        // A verdict for another answer changes nothing; the right one resolves the wait.
        let winner = Actor {
            kind: protocol::tunnel::ActorKind::User,
            id: "usr_1".to_owned(),
            name: None,
            device_id: Some("dev_phone".to_owned()),
        };
        sink.shared
            .finish_answer("ans_other", Err("not for this wait".to_owned()));
        sink.shared.finish_answer(
            &answer_id,
            Ok(AnswerReply {
                status: AnswerStatus::AlreadyResolved,
                resolved_by: Some(winner.clone()),
                message: "this interrupt was already answered".to_owned(),
            }),
        );
        let reply = waiting.await.expect("task").expect("a verdict");
        assert_eq!(reply.status, AnswerStatus::AlreadyResolved);
        assert_eq!(reply.resolved_by, Some(winner));
    }

    /// A prompt typed here is a durable `prompt_submit` up frame, in order, and nothing else.
    #[tokio::test]
    async fn a_prompt_submit_is_queued_up_in_order_after_what_came_before() {
        let (sink, _dir) = sink();
        let text = |text: &str| {
            vec![ContentPart::Text {
                text: text.to_owned(),
            }]
        };
        sink.prompt_submit("th_1", "tk_1", "msg_1", &text("first"))
            .expect("first");
        sink.prompt_submit("th_1", "tk_2", "msg_2", &text("second"))
            .expect("second");
        let rows = sink.shared.store.outbound_since(0).expect("outbox");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].body["t"], "prompt_submit");
        assert_eq!(rows[0].body["thread_id"], "th_1");
        assert_eq!(rows[0].body["task_id"], "tk_1");
        assert_eq!(rows[0].body["message_id"], "msg_1");
        assert_eq!(rows[0].body["parts"][0]["text"], "first");
        assert_eq!(rows[1].body["task_id"], "tk_2");
        assert!(rows[0].seq < rows[1].seq);
    }

    /// No verdict in time is an error, never an acceptance, and the waiter is cleaned up.
    #[tokio::test]
    async fn an_interrupt_answer_without_a_verdict_is_an_error() {
        let (sink, _dir) = sink();
        let result = sink
            .interrupt_answer("th_1", "int_1", &allow(), Duration::from_millis(30))
            .await;
        assert!(result.is_err(), "{result:?}");
        assert!(sink.shared.lock_answers().is_empty());
    }
}
