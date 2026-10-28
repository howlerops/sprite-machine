// TRACER STUB: replaced by M2B-002 follow-ups (the tracer's HTTP side moves to the Thread DO routes; this object only keeps the tracer's threads in memory)
//! The tracer hub's thread state in one in-memory object: the threads and the HTTP idempotency
//! keys. Every method is pure (no IO, time passed in) and returns [`Out`] messages for the
//! Durable Object glue to deliver, so this is unit-tested natively.
//!
//! M2B-002 moved the tunnel out of this object: the runner's WebSocket now ends at the real
//! Machine DO (`machines/`, `do/machine.rs`). This object keeps the tracer's thread store and
//! HTTP API, queues commands by pushing them to the Machine DO ([`Out::Command`]), and takes
//! the runner's events from the Machine DO on its internal route ([`HubState::append_event`]).
//!
//! Stub limits: one object holds every thread; no auth on any route; the thread is created by
//! its first prompt and hosted by the machine named in that prompt; idempotency keys are kept
//! forever in memory; a command whose push to the Machine DO fails is lost.

use std::collections::HashMap;

use protocol::tunnel::{Actor, Command, ContentPart, Outcome};
use protocol::turn_event::{Envelope, MAX_ENVELOPE_BYTES, ResolvedOutcome, format_timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::thread::{ResolveError, ThreadState, event_message};

/// One command for the runner, before the Machine DO gives it a frame `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommand {
    pub idempotency_key: String,
    pub issued_at: String,
    pub command: Command,
}

/// A message for the Durable Object glue to deliver.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Push this command to the machine's Machine DO, which queues it (in order) and sends it
    /// down the tunnel as soon as the runner is connected.
    Command {
        machine_id: String,
        thread_id: String,
        queued: Box<QueuedCommand>,
    },
    /// Send a subscription message (§6) to every subscriber of the thread.
    Subscribers { thread_id: String, message: Value },
}

/// An HTTP error answer.
#[derive(Debug, Clone, PartialEq)]
pub struct HttpError {
    pub status: u16,
    pub body: Value,
}

impl HttpError {
    fn new(status: u16, message: impl Into<String>) -> Self {
        Self {
            status,
            body: json!({ "error": message.into() }),
        }
    }
}

/// `POST /threads/{thread_id}/prompts` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PromptRequest {
    /// The machine that hosts the thread (fixed by the thread's first prompt).
    pub machine_id: String,
    pub task_id: String,
    pub message_id: String,
    pub parts: Vec<ContentPart>,
    pub author: Actor,
}

/// `POST /threads/{thread_id}/interrupts/{interrupt_id}` body.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AnswerRequest {
    pub outcome: Outcome,
    pub resolved_by: Actor,
}

/// The tracer hub.
#[derive(Debug, Default)]
pub struct HubState {
    threads: HashMap<String, ThreadState>,
    /// Idempotency key → the HTTP answer already given (status, body).
    answered: HashMap<String, (u16, Value)>,
}

impl HubState {
    pub fn new() -> Self {
        Self::default()
    }

    /// A thread, if it exists.
    pub fn thread(&self, thread_id: &str) -> Option<&ThreadState> {
        self.threads.get(thread_id)
    }

    /// The catch-up messages for a new subscriber (§6). An unknown thread looks empty; its
    /// events arrive live once its first prompt creates it.
    pub fn subscribe(&self, thread_id: &str, after: u64) -> Vec<Value> {
        match self.threads.get(thread_id) {
            Some(thread) => thread.catch_up(after),
            None => ThreadState::new(thread_id, "").catch_up(after),
        }
    }

    /// `POST /internal/threads/{thread_id}/events` (the Machine DO appends what its runner
    /// sent, after it has checked the thread is hosted by that machine and the frame's
    /// sequence). Answers like the Thread DO's route: `200 {"stored":[...]}`, `422
    /// invalid_sequence`, `413`, `400`, and `403` for a thread this object does not know.
    pub fn append_event(
        &mut self,
        thread_id: &str,
        raw: Value,
        now_ms: u64,
    ) -> Result<(u16, Value, Vec<Out>), HttpError> {
        let size = serde_json::to_vec(&raw).map_or(usize::MAX, |bytes| bytes.len());
        if size > MAX_ENVELOPE_BYTES {
            return Err(HttpError {
                status: 413,
                body: json!({ "error": "too_large", "message": format!("envelope is {size} bytes") }),
            });
        }
        let envelope: Envelope = serde_json::from_value(raw).map_err(|error| HttpError {
            status: 400,
            body: json!({ "error": "invalid_request", "message": error.to_string() }),
        })?;
        if envelope.thread_id != thread_id {
            return Err(HttpError {
                status: 400,
                body: json!({
                    "error": "invalid_request",
                    "message": format!("envelope is for thread {}, not {thread_id}", envelope.thread_id),
                }),
            });
        }
        let Some(thread) = self.threads.get_mut(thread_id) else {
            return Err(HttpError {
                status: 403,
                body: json!({
                    "error": "forbidden",
                    "message": format!("no tracer thread {thread_id}"),
                }),
            });
        };
        let stored = thread.append(envelope, now_ms).map_err(|error| HttpError {
            status: error.status,
            body: error.body(),
        })?;
        let out = vec![Out::Subscribers {
            thread_id: thread_id.to_owned(),
            message: event_message(&stored),
        }];
        Ok((200, json!({ "stored": [stored] }), out))
    }

    /// `POST /threads/{thread_id}/prompts`: queues a `prompt` command for the hosting machine.
    pub fn submit_prompt(
        &mut self,
        thread_id: &str,
        idempotency_key: &str,
        request: PromptRequest,
        now_ms: u64,
    ) -> Result<(u16, Value, Vec<Out>), HttpError> {
        if let Some((status, body)) = self.answered.get(idempotency_key) {
            return Ok((*status, body.clone(), Vec::new()));
        }
        let thread = self
            .threads
            .entry(thread_id.to_owned())
            .or_insert_with(|| ThreadState::new(thread_id, &request.machine_id));
        if thread.machine_id() != request.machine_id {
            return Err(HttpError::new(
                409,
                format!("thread {thread_id} is hosted by {}", thread.machine_id()),
            ));
        }
        let command = Command::Prompt {
            thread_id: thread_id.to_owned(),
            task_id: request.task_id.clone(),
            message_id: request.message_id,
            parts: request.parts,
            author: request.author,
        };
        let out = vec![queue(
            &request.machine_id,
            thread_id,
            idempotency_key,
            command,
            now_ms,
        )];
        let body = json!({ "task_id": request.task_id, "queued": true });
        self.answered
            .insert(idempotency_key.to_owned(), (202, body.clone()));
        Ok((202, body, out))
    }

    /// `POST /threads/{thread_id}/interrupts/{interrupt_id}`: first answer wins (§4.5). Appends
    /// `InterruptResolved`, then queues the `approval` command for the runner.
    pub fn answer_interrupt(
        &mut self,
        thread_id: &str,
        interrupt_id: &str,
        idempotency_key: &str,
        request: AnswerRequest,
        now_ms: u64,
    ) -> Result<(u16, Value, Vec<Out>), HttpError> {
        if let Some((status, body)) = self.answered.get(idempotency_key) {
            return Ok((*status, body.clone(), Vec::new()));
        }
        let thread = self
            .threads
            .get_mut(thread_id)
            .ok_or_else(|| HttpError::new(404, format!("no thread {thread_id}")))?;
        let outcome = match &request.outcome {
            Outcome::Selected { option_id } => ResolvedOutcome::Selected {
                option_id: option_id.clone(),
            },
            Outcome::Submitted { data } => ResolvedOutcome::Submitted { data: data.clone() },
            Outcome::Declined => ResolvedOutcome::Declined,
            Outcome::Cancelled => ResolvedOutcome::Cancelled,
        };
        let resolved = thread
            .resolve_interrupt(interrupt_id, outcome, request.resolved_by.clone(), now_ms)
            .map_err(|error| match error {
                ResolveError::NotFound => {
                    HttpError::new(404, format!("no interrupt {interrupt_id}"))
                }
                ResolveError::AlreadyResolved { resolved_by } => HttpError {
                    status: 409,
                    body: json!({ "error": "already resolved", "resolved_by": resolved_by }),
                },
                ResolveError::Rejected(error) => HttpError {
                    status: error.status,
                    body: error.body(),
                },
            })?;
        let machine_id = thread.machine_id().to_owned();
        let mut out = vec![Out::Subscribers {
            thread_id: thread_id.to_owned(),
            message: event_message(&resolved.envelope),
        }];
        let command = Command::Approval {
            thread_id: thread_id.to_owned(),
            task_id: resolved.task_id,
            interrupt_id: interrupt_id.to_owned(),
            outcome: request.outcome,
            resolved_by: request.resolved_by,
        };
        out.push(queue(
            &machine_id,
            thread_id,
            idempotency_key,
            command,
            now_ms,
        ));
        let body = json!({ "seq": resolved.envelope.seq });
        self.answered
            .insert(idempotency_key.to_owned(), (200, body.clone()));
        Ok((200, body, out))
    }
}

fn queue(
    machine_id: &str,
    thread_id: &str,
    idempotency_key: &str,
    command: Command,
    now_ms: u64,
) -> Out {
    Out::Command {
        machine_id: machine_id.to_owned(),
        thread_id: thread_id.to_owned(),
        queued: Box::new(QueuedCommand {
            idempotency_key: idempotency_key.to_owned(),
            issued_at: format_timestamp(now_ms),
            command,
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tunnel::ActorKind;
    use protocol::turn_event::{
        Interrupt, InterruptKind, RunFinished, RunOutcome, RunReason, RunStarted,
    };

    fn user() -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "usr_1".into(),
            name: None,
            device_id: None,
        }
    }

    fn envelope(event: impl Into<protocol::turn_event::Event>) -> Value {
        serde_json::to_value(Envelope::new(
            "th_1",
            Some("tk_1".into()),
            Some("run_1".into()),
            "2026-10-27T22:00:00.000Z",
            event,
        ))
        .expect("json")
    }

    fn user_message() -> Value {
        serde_json::to_value(Envelope::new(
            "th_1",
            Some("tk_1".into()),
            None,
            "2026-10-27T22:00:00.000Z",
            protocol::turn_event::UserMessage {
                message_id: "msg_u1".into(),
                parts: vec![ContentPart::Text { text: "hi".into() }],
                author: user(),
            },
        ))
        .expect("json")
    }

    fn prompt(hub: &mut HubState) {
        let (status, _, out) = hub
            .submit_prompt(
                "th_1",
                "idk_p1",
                PromptRequest {
                    machine_id: "mch_1".into(),
                    task_id: "tk_1".into(),
                    message_id: "msg_u1".into(),
                    parts: vec![ContentPart::Text { text: "hi".into() }],
                    author: user(),
                },
                0,
            )
            .expect("prompt accepted");
        assert_eq!(status, 202);
        assert!(matches!(
            &out[..],
            [Out::Command { machine_id, thread_id, queued }]
                if machine_id == "mch_1"
                    && thread_id == "th_1"
                    && queued.idempotency_key == "idk_p1"
                    && matches!(queued.command, Command::Prompt { .. })
        ));
    }

    #[test]
    fn prompt_event_interrupt_answer_round_trip() {
        let mut hub = HubState::new();
        prompt(&mut hub);

        let started = RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        };
        // The tracer's runner emits UserMessage itself (the hub only does for real prompts).
        hub.append_event("th_1", user_message(), 0)
            .expect("user message");
        let (status, body, out) = hub
            .append_event("th_1", envelope(started), 0)
            .expect("started");
        assert_eq!(status, 200);
        assert_eq!(body["stored"][0]["seq"], 2);
        assert!(
            matches!(&out[..], [Out::Subscribers { message, .. }] if message["envelope"]["seq"] == 2)
        );

        let interrupt = Interrupt {
            interrupt_id: "int_1".into(),
            kind: InterruptKind::Approval,
            title: "Allow?".into(),
            message: None,
            expires_at: "2026-10-27T22:10:00.000Z".into(),
            tool_call_id: None,
            options: None,
            response_schema: None,
            url: None,
        };
        hub.append_event("th_1", envelope(interrupt), 0)
            .expect("interrupt");
        let paused = RunFinished {
            outcome: RunOutcome::Interrupt {
                interrupt_ids: vec!["int_1".into()],
            },
        };
        hub.append_event("th_1", envelope(paused), 0)
            .expect("paused");

        let answer = || AnswerRequest {
            outcome: Outcome::Selected {
                option_id: "allow".into(),
            },
            resolved_by: user(),
        };
        let (status, body, out) = hub
            .answer_interrupt("th_1", "int_1", "idk_a1", answer(), 0)
            .expect("answer accepted");
        assert_eq!((status, &body), (200, &json!({"seq": 5})));
        assert!(matches!(
            &out[..],
            [
                Out::Subscribers { .. },
                Out::Command { queued, .. }
            ] if matches!(queued.command, Command::Approval { .. })
        ));
        // Same key: same answer, no new effect. Other key: 409.
        let (_, again, out) = hub
            .answer_interrupt("th_1", "int_1", "idk_a1", answer(), 0)
            .expect("idempotent");
        assert_eq!(again, body);
        assert!(out.is_empty());
        let conflict = hub
            .answer_interrupt("th_1", "int_1", "idk_a2", answer(), 0)
            .expect_err("second answer");
        assert_eq!(conflict.status, 409);
        assert_eq!(hub.thread("th_1").map(ThreadState::head_seq), Some(5));
    }

    #[test]
    fn events_are_refused_like_the_thread_dos_internal_route_does() {
        let mut hub = HubState::new();
        let started = || RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        };
        // A thread nobody prompted: forbidden.
        let error = hub
            .append_event("th_1", envelope(started()), 0)
            .expect_err("unknown thread");
        assert_eq!(error.status, 403);

        prompt(&mut hub);
        // Not an envelope: 400. For another thread than the path names: 400.
        assert_eq!(
            hub.append_event("th_1", json!({"x": 1}), 0)
                .expect_err("bad")
                .status,
            400
        );
        let mut other = envelope(started());
        other["thread_id"] = json!("th_2");
        assert_eq!(
            hub.append_event("th_1", other, 0)
                .expect_err("other")
                .status,
            400
        );
        // A §5 violation (RunStarted before UserMessage): 422 invalid_sequence.
        let error = hub
            .append_event("th_1", envelope(started()), 0)
            .expect_err("rule 6");
        assert_eq!(
            (error.status, &error.body["error"]),
            (422, &json!("invalid_sequence"))
        );
        // Too large: 413.
        let mut big = envelope(started());
        big["pad"] = json!("x".repeat(MAX_ENVELOPE_BYTES));
        assert_eq!(
            hub.append_event("th_1", big, 0).expect_err("big").status,
            413
        );
    }
}
