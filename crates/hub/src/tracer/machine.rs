// TRACER STUB: replaced by M2B-002 (Machine DO: durable outbox, resume tokens, auth, presence)
//! In-memory machine (tunnel endpoint) state for the tracer bullet, built on the pure
//! [`Sender`] / [`Receiver`] / [`DedupeWindow`] from `protocol::tunnel` (tunnel.md §3–§4).
//!
//! Handles the MVP subset (tunnel.md §7): `hello` → `welcome`, `event` → append + `ack`,
//! `ack` of commands, `ping` → `pong`, and the down stream of `command{prompt, approval}`.
//!
//! Stub limits: no auth (the machine id comes from the URL, not a credential), outboxes and
//! resume tokens live in memory only, acks are sent per frame (no 200 ms batching), no
//! presence, no 7-day command expiry, `thread_open` / `command_result` / `presence` ignored.

use protocol::tunnel::{CloseCode, Command, DedupeWindow, Disposition, Frame, Receiver, Sender};
use protocol::tunnel::{PROTO_VERSION, Verdict};

/// Heartbeat interval announced in `welcome`.
pub const HEARTBEAT_MS: u64 = 20_000;

/// Unacked frames allowed on the wire (tunnel.md §4.2 default).
pub const MAX_INFLIGHT: u32 = 256;

/// One queued down-stream command, before it gets its frame `seq`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedCommand {
    pub idempotency_key: String,
    pub issued_at: String,
    pub command: Command,
}

/// What the tunnel layer should do after a frame from the runner.
#[derive(Debug, Clone, PartialEq)]
pub enum TunnelAction {
    /// Send this frame to the runner.
    Send(Box<Frame>),
    /// Append this envelope (JSON as received) to its thread; then the frame is acked.
    Append {
        frame_seq: u64,
        event_id: String,
        envelope: serde_json::Value,
    },
    /// Send `close{code}` and close the socket with `4000 + n`.
    Close(CloseCode, String),
}

impl TunnelAction {
    /// A [`TunnelAction::Send`] of `frame`.
    pub fn send(frame: Frame) -> Self {
        Self::Send(Box::new(frame))
    }
}

/// One machine's tunnel state.
#[derive(Debug, Clone)]
pub struct MachineState {
    machine_id: String,
    connected: bool,
    up: Receiver,
    down: Sender<QueuedCommand>,
    event_ids: DedupeWindow,
    token_counter: u64,
    resume_token: Option<String>,
}

impl MachineState {
    /// A machine that has never connected.
    pub fn new(machine_id: impl Into<String>) -> Self {
        Self {
            machine_id: machine_id.into(),
            connected: false,
            up: Receiver::fresh(),
            down: Sender::new(u64::from(MAX_INFLIGHT)),
            event_ids: DedupeWindow::event_ids(),
            token_counter: 0,
            resume_token: None,
        }
    }

    /// `true` between a `welcome` and the socket closing.
    pub fn is_connected(&self) -> bool {
        self.connected
    }

    /// The socket closed: commands queue until the next `hello`.
    pub fn on_disconnect(&mut self) {
        self.connected = false;
    }

    /// Queues a command for the runner. Returns the frames to send now (none while the runner
    /// is offline: they go out after the next `welcome`).
    pub fn enqueue(&mut self, command: QueuedCommand) -> Vec<Frame> {
        self.down.enqueue(command);
        self.drain()
    }

    /// Handles one frame from the runner.
    pub fn on_frame(&mut self, frame: Frame, now_ms: u64) -> Vec<TunnelAction> {
        match frame {
            Frame::Hello {
                proto,
                machine_id,
                resume_token,
                last_down_seq,
                ..
            } => self.on_hello(proto, &machine_id, resume_token, last_down_seq),
            Frame::Event {
                seq,
                event_id,
                envelope,
            } => {
                if !self.connected {
                    return vec![protocol_error("event before hello")];
                }
                match self.up.on_frame(seq) {
                    Err(error) => vec![protocol_error(&error.to_string())],
                    Ok(Verdict::Duplicate { ack }) => {
                        vec![TunnelAction::send(Frame::Ack { seq: ack })]
                    }
                    Ok(Verdict::Process) => match self.event_ids.observe(&event_id, now_ms) {
                        // Seen on an earlier stream (§4.4): ack without appending again.
                        Disposition::Duplicate => self.ack_now(),
                        Disposition::New => vec![TunnelAction::Append {
                            frame_seq: seq,
                            event_id,
                            envelope,
                        }],
                    },
                }
            }
            Frame::Ack { seq } => match self.down.on_ack(seq) {
                Ok(()) => self.drain().into_iter().map(TunnelAction::send).collect(),
                Err(error) => vec![protocol_error(&error.to_string())],
            },
            Frame::Ping => vec![TunnelAction::send(Frame::Pong)],
            Frame::Pong | Frame::Close { .. } => Vec::new(),
            // Not needed by the tracer (tunnel.md §7).
            Frame::ThreadOpen { .. } | Frame::CommandResult { .. } | Frame::Presence { .. } => {
                Vec::new()
            }
            Frame::Welcome { .. } | Frame::Command { .. } | Frame::Nack { .. } => {
                vec![protocol_error("down-stream frame sent by the runner")]
            }
        }
    }

    /// The ack to send after an `Append` action was applied (cumulative).
    pub fn ack_now(&mut self) -> Vec<TunnelAction> {
        self.up
            .take_ack()
            .map(|seq| TunnelAction::send(Frame::Ack { seq }))
            .into_iter()
            .collect()
    }

    fn on_hello(
        &mut self,
        proto: u32,
        machine_id: &str,
        resume_token: Option<String>,
        last_down_seq: u64,
    ) -> Vec<TunnelAction> {
        if proto != PROTO_VERSION {
            return vec![TunnelAction::Close(
                CloseCode::VersionUnsupported,
                format!("tunnel proto {proto} is not supported"),
            )];
        }
        if machine_id != self.machine_id {
            return vec![TunnelAction::Close(
                CloseCode::AuthInvalid,
                "hello.machine_id does not match the tunnel URL".to_owned(),
            )];
        }
        let resumed = resume_token.is_some() && resume_token == self.resume_token;
        let cursor = if resumed { last_down_seq } else { 0 };
        if let Err(error) = self.down.on_welcome(resumed, cursor) {
            return vec![protocol_error(&error.to_string())];
        }
        if !resumed {
            self.up = Receiver::fresh();
        }
        self.token_counter += 1;
        let token = format!("rt_tracer_{}_{}", self.machine_id, self.token_counter);
        self.resume_token = Some(token.clone());
        self.connected = true;
        let mut actions = vec![TunnelAction::send(Frame::Welcome {
            proto: PROTO_VERSION,
            resumed,
            resume_token: token,
            last_up_seq: self.up.last(),
            heartbeat_ms: HEARTBEAT_MS,
            max_inflight: MAX_INFLIGHT,
        })];
        actions.extend(self.drain().into_iter().map(TunnelAction::send));
        actions
    }

    /// Every command frame that may go on the wire now.
    fn drain(&mut self) -> Vec<Frame> {
        if !self.connected {
            return Vec::new();
        }
        let mut frames = Vec::new();
        while let Some((seq, queued)) = self.down.next_to_send() {
            frames.push(Frame::Command {
                seq,
                idempotency_key: queued.idempotency_key,
                issued_at: queued.issued_at,
                command: queued.command,
            });
        }
        frames
    }
}

fn protocol_error(message: &str) -> TunnelAction {
    TunnelAction::Close(CloseCode::ProtocolError, message.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tunnel::Capabilities;
    use serde_json::json;

    fn hello(token: Option<&str>) -> Frame {
        Frame::Hello {
            proto: 1,
            machine_id: "mch_1".into(),
            runner_version: "0.0.0".into(),
            resume_token: token.map(str::to_owned),
            last_down_seq: 0,
            capabilities: Capabilities {
                host_mode: "local".into(),
                harnesses: vec![],
                max_sessions: 1,
                spawn: vec![],
                features: vec![],
            },
        }
    }

    fn cancel() -> QueuedCommand {
        QueuedCommand {
            idempotency_key: "idk_1".into(),
            issued_at: "2026-10-27T22:00:00.000Z".into(),
            command: Command::Cancel {
                thread_id: "th_1".into(),
                task_id: "tk_1".into(),
            },
        }
    }

    #[test]
    fn commands_queue_offline_and_flush_after_welcome() {
        let mut machine = MachineState::new("mch_1");
        assert!(machine.enqueue(cancel()).is_empty());
        let actions = machine.on_frame(hello(None), 0);
        assert!(matches!(
            &actions[0],
            TunnelAction::Send(frame) if matches!(**frame, Frame::Welcome { resumed: false, .. })
        ));
        assert!(matches!(
            &actions[1],
            TunnelAction::Send(frame) if matches!(**frame, Frame::Command { seq: 1, .. })
        ));
        assert!(machine.on_frame(Frame::Ack { seq: 1 }, 0).is_empty());
        assert_eq!(
            machine.enqueue(cancel()).first(),
            Some(&Frame::Command {
                seq: 2,
                idempotency_key: "idk_1".into(),
                issued_at: "2026-10-27T22:00:00.000Z".into(),
                command: cancel().command,
            })
        );
    }

    #[test]
    fn events_are_appended_once_and_acked() {
        let mut machine = MachineState::new("mch_1");
        machine.on_frame(hello(None), 0);
        let event = |seq| Frame::Event {
            seq,
            event_id: "evt_1".into(),
            envelope: json!({}),
        };
        assert!(matches!(
            machine.on_frame(event(1), 0).as_slice(),
            [TunnelAction::Append { frame_seq: 1, .. }]
        ));
        assert_eq!(
            machine.ack_now(),
            vec![TunnelAction::send(Frame::Ack { seq: 1 })]
        );
        // A retransmit is a duplicate: re-ack, no append.
        assert_eq!(
            machine.on_frame(event(1), 0),
            vec![TunnelAction::send(Frame::Ack { seq: 1 })]
        );
        // A gap closes the tunnel.
        assert!(matches!(
            machine.on_frame(event(5), 0).as_slice(),
            [TunnelAction::Close(CloseCode::ProtocolError, _)]
        ));
    }

    #[test]
    fn ping_gets_pong_and_wrong_machine_is_refused() {
        let mut machine = MachineState::new("mch_other");
        assert!(matches!(
            machine.on_frame(hello(None), 0).as_slice(),
            [TunnelAction::Close(CloseCode::AuthInvalid, _)]
        ));
        assert_eq!(
            machine.on_frame(Frame::Ping, 0),
            vec![TunnelAction::send(Frame::Pong)]
        );
    }
}
