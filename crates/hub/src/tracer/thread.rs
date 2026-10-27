// TRACER STUB: replaced by M2-008 (Thread DO: SQLite event log, validate_sequence on append, interrupts, catch-up)
//! In-memory thread state for the tracer bullet: the event log with `seq` assigned by
//! [`ThreadSeq`] (turn-event.md §3), first-answer-wins interrupts (§4.5), and the
//! subscription catch-up messages (§6).
//!
//! Stub limits: nothing is persisted (a Durable Object eviction loses the thread), appends are
//! not checked against the §5 ordering rules (`validate_sequence` is M1A-006a), there is no
//! redaction and no state snapshots (the `snapshot` message always has empty `states`).

use std::collections::HashMap;

use protocol::turn_event::{
    Actor, Envelope, Event, InterruptResolved, ResolvedOutcome, StampError, ThreadSeq,
};
use serde_json::{Value, json};

/// One open or resolved interrupt.
#[derive(Debug, Clone)]
struct InterruptState {
    task_id: Option<String>,
    resolved_by: Option<Actor>,
}

/// The `seq` span of one run: from its `RunStarted` to its terminal event (if any yet).
#[derive(Debug, Clone, Copy)]
struct RunSpan {
    start: u64,
    end: Option<u64>,
}

/// Why an interrupt answer was not accepted.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// No such interrupt in this thread.
    NotFound,
    /// Another answer was accepted first (HTTP 409 naming `resolved_by`, §4.5).
    AlreadyResolved { resolved_by: Actor },
}

/// An accepted interrupt answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The stamped `InterruptResolved` envelope, already in the log.
    pub envelope: Envelope,
    /// The task the interrupt belongs to.
    pub task_id: String,
}

/// One thread: its log, its interrupts and the machine that hosts it.
#[derive(Debug, Clone)]
pub struct ThreadState {
    thread_id: String,
    machine_id: String,
    seq: ThreadSeq,
    log: Vec<Envelope>,
    interrupts: HashMap<String, InterruptState>,
    runs: HashMap<String, RunSpan>,
}

impl ThreadState {
    /// A new, empty thread hosted by `machine_id`.
    pub fn new(thread_id: impl Into<String>, machine_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            machine_id: machine_id.into(),
            seq: ThreadSeq::new(),
            log: Vec::new(),
            interrupts: HashMap::new(),
            runs: HashMap::new(),
        }
    }

    /// The machine that hosts this thread.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// The last stored `seq` (0 when empty).
    pub fn head_seq(&self) -> u64 {
        self.seq.last()
    }

    /// Every stored envelope, in `seq` order.
    pub fn log(&self) -> &[Envelope] {
        &self.log
    }

    /// Appends a producer envelope (`seq: 0`): assigns the next `seq` and returns the stored
    /// envelope.
    pub fn append(&mut self, envelope: Envelope) -> Result<Envelope, StampError> {
        let stored = self.seq.stamp(envelope)?;
        self.track(&stored);
        self.log.push(stored.clone());
        Ok(stored)
    }

    /// Accepts the first answer to `interrupt_id` and appends `InterruptResolved` (hub-produced,
    /// scope: task). Later answers get [`ResolveError::AlreadyResolved`].
    pub fn resolve_interrupt(
        &mut self,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        resolved_by: Actor,
        ts: String,
    ) -> Result<Resolved, ResolveError> {
        let state = self
            .interrupts
            .get(interrupt_id)
            .ok_or(ResolveError::NotFound)?;
        if let Some(resolved_by) = &state.resolved_by {
            return Err(ResolveError::AlreadyResolved {
                resolved_by: resolved_by.clone(),
            });
        }
        let task_id = state.task_id.clone().ok_or(ResolveError::NotFound)?;
        let envelope = Envelope::new(
            self.thread_id.clone(),
            Some(task_id.clone()),
            None,
            ts,
            InterruptResolved {
                interrupt_id: interrupt_id.to_owned(),
                outcome,
                resolved_by,
            },
        );
        // The envelope is built here with seq 0, so stamping cannot fail.
        let envelope = self.append(envelope).map_err(|_| ResolveError::NotFound)?;
        Ok(Resolved { envelope, task_id })
    }

    /// The subscription catch-up for `?after=<after>` (turn-event.md §6): `hello`, `snapshot`,
    /// replayed `event`s from the run boundary, then `live`.
    pub fn catch_up(&self, after: u64) -> Vec<Value> {
        let head = self.head_seq();
        let replay_from = self.replay_from(after);
        let mut messages = vec![
            json!({ "type": "hello", "head_seq": head, "replay_from": replay_from }),
            json!({ "type": "snapshot", "at_seq": replay_from.saturating_sub(1), "states": {} }),
        ];
        messages.extend(
            self.log
                .iter()
                .filter(|envelope| envelope.seq >= replay_from)
                .map(event_message),
        );
        messages.push(json!({ "type": "live" }));
        messages
    }

    /// `R = min(boundary, after + 1)`, where the boundary is the `RunStarted` seq of the run
    /// open at `after` (§6 step 1).
    fn replay_from(&self, after: u64) -> u64 {
        let boundary = self
            .runs
            .values()
            .filter(|span| span.start <= after && span.end.is_none_or(|end| end > after))
            .map(|span| span.start)
            .min()
            .unwrap_or(after + 1);
        boundary.min(after + 1)
    }

    /// Updates interrupt and run bookkeeping for a stored envelope.
    fn track(&mut self, envelope: &Envelope) {
        match &envelope.event {
            Event::Interrupt(interrupt) => {
                self.interrupts.insert(
                    interrupt.interrupt_id.clone(),
                    InterruptState {
                        task_id: envelope.task_id.clone(),
                        resolved_by: None,
                    },
                );
            }
            Event::InterruptResolved(resolved) => {
                if let Some(state) = self.interrupts.get_mut(&resolved.interrupt_id) {
                    state.resolved_by = Some(resolved.resolved_by.clone());
                }
            }
            Event::RunStarted(_) => {
                if let Some(run_id) = &envelope.run_id {
                    self.runs.insert(
                        run_id.clone(),
                        RunSpan {
                            start: envelope.seq,
                            end: None,
                        },
                    );
                }
            }
            Event::RunFinished(_) | Event::RunError(_) => {
                if let Some(span) = envelope
                    .run_id
                    .as_ref()
                    .and_then(|run_id| self.runs.get_mut(run_id))
                {
                    span.end = Some(envelope.seq);
                }
            }
            _ => {}
        }
    }
}

/// The `{"type":"event","envelope":…}` subscription message (§6).
pub fn event_message(envelope: &Envelope) -> Value {
    json!({ "type": "event", "envelope": envelope })
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::turn_event::{
        ActorKind, Interrupt, InterruptKind, RunFinished, RunOutcome, RunReason, RunStarted,
        TextMessageEnd,
    };

    const TS: &str = "2026-10-27T22:00:00.000Z";

    fn run_env(run: &str, event: impl Into<Event>) -> Envelope {
        Envelope::new("th_1", Some("tk_1".into()), Some(run.into()), TS, event)
    }

    fn started(run: &str) -> Envelope {
        run_env(
            run,
            RunStarted {
                reason: RunReason::Prompt,
                parent_run_id: None,
                agent_id: None,
                machine_id: None,
            },
        )
    }

    fn user() -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "usr_1".into(),
            name: None,
            device_id: None,
        }
    }

    fn interrupt() -> Envelope {
        run_env(
            "run_1",
            Interrupt {
                interrupt_id: "int_1".into(),
                kind: InterruptKind::Approval,
                title: "Allow?".into(),
                message: None,
                expires_at: TS.into(),
                tool_call_id: None,
                options: None,
                response_schema: None,
                url: None,
            },
        )
    }

    #[test]
    fn append_assigns_gap_free_seq_and_refuses_stamped_envelopes() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        assert_eq!(thread.append(started("run_1")).map(|e| e.seq), Ok(1));
        let end = run_env(
            "run_1",
            TextMessageEnd {
                message_id: "m".into(),
            },
        );
        assert_eq!(thread.append(end.clone()).map(|e| e.seq), Ok(2));
        let mut stamped = end;
        stamped.seq = 7;
        assert!(thread.append(stamped).is_err());
        assert_eq!(thread.head_seq(), 2);
    }

    #[test]
    fn first_answer_wins() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        thread.append(started("run_1")).expect("append");
        thread.append(interrupt()).expect("append");
        let selected = || ResolvedOutcome::Selected {
            option_id: "allow".into(),
        };
        let resolved = thread
            .resolve_interrupt("int_1", selected(), user(), TS.into())
            .expect("first answer accepted");
        assert_eq!(resolved.envelope.seq, 3);
        assert_eq!(resolved.task_id, "tk_1");
        assert_eq!(resolved.envelope.run_id, None);
        assert_eq!(
            thread.resolve_interrupt("int_1", selected(), user(), TS.into()),
            Err(ResolveError::AlreadyResolved {
                resolved_by: user()
            })
        );
        assert_eq!(
            thread.resolve_interrupt("int_9", selected(), user(), TS.into()),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn catch_up_replays_from_the_open_run_boundary() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        thread.append(started("run_1")).expect("append"); // 1
        let end = run_env(
            "run_1",
            TextMessageEnd {
                message_id: "m".into(),
            },
        );
        thread.append(end).expect("append"); // 2
        thread.append(interrupt()).expect("append"); // 3

        // Mid-run: replay starts at the run's RunStarted.
        let messages = thread.catch_up(2);
        assert_eq!(
            messages[0],
            json!({"type":"hello","head_seq":3,"replay_from":1})
        );
        assert_eq!(messages[1]["at_seq"], json!(0));
        assert_eq!(messages.len(), 2 + 3 + 1);
        assert_eq!(messages.last(), Some(&json!({"type":"live"})));

        let finish = run_env(
            "run_1",
            RunFinished {
                outcome: RunOutcome::Interrupt {
                    interrupt_ids: vec!["int_1".into()],
                },
            },
        );
        thread.append(finish).expect("append"); // 4
        // After the run ended: replay from after + 1.
        let messages = thread.catch_up(4);
        assert_eq!(
            messages[0],
            json!({"type":"hello","head_seq":4,"replay_from":5})
        );
        assert_eq!(messages.len(), 3);
    }
}
