// TRACER STUB: replaced by M2-008 + M2B-002 (Thread DO HTTP API and Machine DO → Thread DO routing as separate objects)
//! The tracer hub's whole state in one in-memory object: machines (tunnel ends) and threads,
//! plus the HTTP idempotency keys. Every method is pure (no IO, time passed in) and returns
//! [`Out`] messages for the Durable Object glue to deliver, so this is unit-tested natively.
//!
//! Stub limits: one object holds every machine and thread (the real hub has one Machine DO per
//! runner and one Thread DO per thread); no auth on any route; the thread is created by its
//! first prompt and hosted by the machine named in that prompt; idempotency keys are kept
//! forever in memory.

use std::collections::HashMap;

use protocol::tunnel::{Actor, CloseCode, Command, ContentPart, Frame, NackCode, Outcome};
use protocol::turn_event::{Envelope, MAX_ENVELOPE_BYTES, ResolvedOutcome, format_timestamp};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use super::machine::{MachineState, QueuedCommand, TunnelAction};
use super::thread::{ResolveError, ThreadState, event_message};

/// A message for the Durable Object glue to deliver.
#[derive(Debug, Clone, PartialEq)]
pub enum Out {
    /// Send a frame on the machine's tunnel socket.
    Tunnel {
        machine_id: String,
        frame: Box<Frame>,
    },
    /// Send `close{code}` and close the machine's tunnel socket with `4000 + n`.
    CloseTunnel {
        machine_id: String,
        code: CloseCode,
        message: String,
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
    machines: HashMap<String, MachineState>,
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

    /// A tunnel socket closed.
    pub fn tunnel_closed(&mut self, machine_id: &str) {
        if let Some(machine) = self.machines.get_mut(machine_id) {
            machine.on_disconnect();
        }
    }

    /// One text message from a machine's tunnel socket.
    pub fn tunnel_message(&mut self, machine_id: &str, text: &str, now_ms: u64) -> Vec<Out> {
        let close = |message: String| {
            vec![Out::CloseTunnel {
                machine_id: machine_id.to_owned(),
                code: CloseCode::ProtocolError,
                message,
            }]
        };
        if text.len() > protocol::tunnel::MAX_FRAME_BYTES {
            return close("frame larger than 1 MiB".to_owned());
        }
        let frame: Frame = match serde_json::from_str(text) {
            Ok(frame) => frame,
            Err(error) => return close(format!("invalid frame: {error}")),
        };
        let machine = self
            .machines
            .entry(machine_id.to_owned())
            .or_insert_with(|| MachineState::new(machine_id));
        let actions = machine.on_frame(frame, now_ms);
        let mut out = Vec::new();
        for action in actions {
            match action {
                TunnelAction::Send(frame) => out.push(Out::Tunnel {
                    machine_id: machine_id.to_owned(),
                    frame,
                }),
                TunnelAction::Close(code, message) => out.push(Out::CloseTunnel {
                    machine_id: machine_id.to_owned(),
                    code,
                    message,
                }),
                TunnelAction::Append {
                    frame_seq,
                    envelope,
                    ..
                } => out.extend(self.append_from_tunnel(machine_id, frame_seq, envelope, now_ms)),
            }
        }
        out
    }

    /// Validates and appends one envelope from a machine, then acks (or nacks) its frame.
    fn append_from_tunnel(
        &mut self,
        machine_id: &str,
        frame_seq: u64,
        raw: Value,
        now_ms: u64,
    ) -> Vec<Out> {
        let mut out = Vec::new();
        let tunnel = |frame: Frame| Out::Tunnel {
            machine_id: machine_id.to_owned(),
            frame: Box::new(frame),
        };
        let nack = |code, message: String| {
            tunnel(Frame::Nack {
                seq: frame_seq,
                code,
                message,
            })
        };
        let size = serde_json::to_vec(&raw)
            .map(|bytes| bytes.len())
            .unwrap_or(0);
        let result = if size > MAX_ENVELOPE_BYTES {
            Err(nack(
                NackCode::TooLarge,
                format!("envelope is {size} bytes"),
            ))
        } else {
            match serde_json::from_value::<Envelope>(raw) {
                Err(error) => Err(nack(NackCode::InvalidEnvelope, error.to_string())),
                Ok(envelope) => match self.threads.get_mut(&envelope.thread_id) {
                    Some(thread) if thread.machine_id() == machine_id => {
                        let thread_id = envelope.thread_id.clone();
                        thread
                            .append(envelope, now_ms)
                            .map(|stored| Out::Subscribers {
                                thread_id,
                                message: event_message(&stored),
                            })
                            .map_err(|error| nack(NackCode::InvalidEnvelope, error.to_string()))
                    }
                    _ => Err(nack(
                        NackCode::Forbidden,
                        format!(
                            "thread {} is not hosted by {machine_id}",
                            envelope.thread_id
                        ),
                    )),
                },
            }
        };
        match result {
            Ok(broadcast) => out.push(broadcast),
            Err(nack) => out.push(nack),
        }
        if let Some(machine) = self.machines.get_mut(machine_id) {
            for action in machine.ack_now() {
                if let TunnelAction::Send(frame) = action {
                    out.push(tunnel(*frame));
                }
            }
        }
        out
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
        let out = self.queue(&request.machine_id, idempotency_key, command, now_ms);
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
        out.extend(self.queue(&machine_id, idempotency_key, command, now_ms));
        let body = json!({ "seq": resolved.envelope.seq });
        self.answered
            .insert(idempotency_key.to_owned(), (200, body.clone()));
        Ok((200, body, out))
    }

    fn queue(
        &mut self,
        machine_id: &str,
        idempotency_key: &str,
        command: Command,
        now_ms: u64,
    ) -> Vec<Out> {
        let machine = self
            .machines
            .entry(machine_id.to_owned())
            .or_insert_with(|| MachineState::new(machine_id));
        machine
            .enqueue(QueuedCommand {
                idempotency_key: idempotency_key.to_owned(),
                issued_at: format_timestamp(now_ms),
                command,
            })
            .into_iter()
            .map(|frame| Out::Tunnel {
                machine_id: machine_id.to_owned(),
                frame: Box::new(frame),
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tunnel::{ActorKind, Capabilities};
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

    fn hello() -> String {
        serde_json::to_string(&Frame::Hello {
            proto: 1,
            machine_id: "mch_1".into(),
            runner_version: "0".into(),
            resume_token: None,
            last_down_seq: 0,
            capabilities: Capabilities {
                host_mode: "local".into(),
                harnesses: vec![],
                max_sessions: 1,
                spawn: vec![],
                features: vec![],
            },
        })
        .expect("json")
    }

    fn event_frame(seq: u64, event: impl Into<protocol::turn_event::Event>) -> String {
        let envelope = Envelope::new(
            "th_1",
            Some("tk_1".into()),
            Some("run_1".into()),
            "2026-10-27T22:00:00.000Z",
            event,
        );
        serde_json::to_string(&Frame::Event {
            seq,
            event_id: format!("evt_{seq}"),
            envelope: serde_json::to_value(envelope).expect("json"),
        })
        .expect("json")
    }

    fn user_message_frame(seq: u64) -> String {
        let envelope = Envelope::new(
            "th_1",
            Some("tk_1".into()),
            None,
            "2026-10-27T22:00:00.000Z",
            protocol::turn_event::UserMessage {
                message_id: "msg_u1".into(),
                parts: vec![ContentPart::Text { text: "hi".into() }],
                author: user(),
            },
        );
        serde_json::to_string(&Frame::Event {
            seq,
            event_id: format!("evt_{seq}"),
            envelope: serde_json::to_value(envelope).expect("json"),
        })
        .expect("json")
    }

    fn frames(out: &[Out]) -> Vec<&Frame> {
        out.iter()
            .filter_map(|o| match o {
                Out::Tunnel { frame, .. } => Some(frame.as_ref()),
                _ => None,
            })
            .collect()
    }

    #[test]
    fn prompt_event_interrupt_answer_round_trip() {
        let mut hub = HubState::new();
        // The prompt is queued while the runner is offline, delivered after welcome.
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
        assert!(out.is_empty());
        let out = hub.tunnel_message("mch_1", &hello(), 0);
        assert!(matches!(
            frames(&out)[..],
            [Frame::Welcome { .. }, Frame::Command { seq: 1, .. }]
        ));

        let started = RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        };
        // The tracer's runner emits UserMessage itself (the hub only does for real prompts).
        hub.tunnel_message("mch_1", &user_message_frame(1), 0);
        let out = hub.tunnel_message("mch_1", &event_frame(2, started), 0);
        assert!(
            matches!(&out[0], Out::Subscribers { message, .. } if message["envelope"]["seq"] == 2)
        );
        assert_eq!(frames(&out), [&Frame::Ack { seq: 2 }]);

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
        hub.tunnel_message("mch_1", &event_frame(3, interrupt), 0);
        let paused = RunFinished {
            outcome: RunOutcome::Interrupt {
                interrupt_ids: vec!["int_1".into()],
            },
        };
        hub.tunnel_message("mch_1", &event_frame(4, paused), 0);

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
            frames(&out)[..],
            [Frame::Command {
                seq: 2,
                command: Command::Approval { .. },
                ..
            }]
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
    fn events_for_threads_of_other_machines_are_nacked() {
        let mut hub = HubState::new();
        hub.tunnel_message("mch_1", &hello(), 0);
        let started = RunStarted {
            reason: RunReason::Prompt,
            parent_run_id: None,
            agent_id: None,
            machine_id: None,
        };
        let out = hub.tunnel_message("mch_1", &event_frame(1, started), 0);
        assert!(matches!(
            frames(&out)[..],
            [
                Frame::Nack {
                    seq: 1,
                    code: NackCode::Forbidden,
                    ..
                },
                Frame::Ack { seq: 1 }
            ]
        ));
        let out = hub.tunnel_message("mch_1", "not json", 0);
        assert!(matches!(
            &out[..],
            [Out::CloseTunnel {
                code: CloseCode::ProtocolError,
                ..
            }]
        ));
    }
}
