// TRACER STUB: replaced by M2B-002 (the tracer hub module goes away; the thread logic is the real `threads::ThreadCore`)
//! The tracer's thread: a thin wrapper over [`ThreadCore`] with an in-memory store (M2-008
//! replaced the MVP-000 stub that lived here). `seq` stamping, the §5 validator, first-answer-wins
//! interrupts and the §6 catch-up are the real ones; what stays tracer-only is that the thread is
//! hosted by one machine and that the tracer's runner (not the hub) emits `UserMessage`.

use protocol::turn_event::{Actor, Envelope, ResolvedOutcome};
use serde_json::Value;

use crate::threads::{ApiError, Resolved, ThreadCore};

pub use crate::threads::event_message;

/// Why an interrupt answer was not accepted.
#[derive(Debug, Clone, PartialEq)]
pub enum ResolveError {
    /// No such interrupt in this thread.
    NotFound,
    /// Another answer was accepted first (HTTP 409 naming `resolved_by`, §4.5).
    AlreadyResolved { resolved_by: Actor },
    /// Any other refusal (410 expired, 400 outcome the interrupt does not allow, ...).
    Rejected(ApiError),
}

/// One thread: its [`ThreadCore`] and the machine that hosts it.
#[derive(Debug)]
pub struct ThreadState {
    machine_id: String,
    core: ThreadCore<crate::threads::MemoryStore>,
}

impl ThreadState {
    /// A new, empty thread hosted by `machine_id`.
    pub fn new(thread_id: impl Into<String>, machine_id: impl Into<String>) -> Self {
        Self {
            machine_id: machine_id.into(),
            core: ThreadCore::new_in_memory(thread_id),
        }
    }

    /// The machine that hosts this thread.
    pub fn machine_id(&self) -> &str {
        &self.machine_id
    }

    /// The last stored `seq` (0 when empty).
    pub fn head_seq(&self) -> u64 {
        self.core.head_seq()
    }

    /// Every stored envelope, in `seq` order.
    pub fn log(&self) -> Vec<Envelope> {
        use crate::threads::ThreadStore;
        self.core
            .store()
            .read_range(1, self.core.head_seq())
            .unwrap_or_default()
    }

    /// Appends a producer envelope (`seq: 0`): assigns the next `seq`, validates it against
    /// turn-event.md §5 and returns the stored envelope.
    pub fn append(&mut self, envelope: Envelope, now_ms: u64) -> Result<Envelope, ApiError> {
        let result = self.core.append(envelope, now_ms);
        // The tracer has no pending-task queue and broadcasts only the stored envelope.
        self.core.take_appended();
        result
    }

    /// Accepts the first answer to `interrupt_id` and appends `InterruptResolved` (hub-produced,
    /// scope: task). Later answers get [`ResolveError::AlreadyResolved`].
    pub fn resolve_interrupt(
        &mut self,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        resolved_by: Actor,
        now_ms: u64,
    ) -> Result<Resolved, ResolveError> {
        let result = self
            .core
            .resolve_interrupt(interrupt_id, outcome, resolved_by, now_ms);
        self.core.take_appended();
        result.map_err(|error| match error.status {
            404 => ResolveError::NotFound,
            409 => match error.resolved_by {
                Some(resolved_by) => ResolveError::AlreadyResolved {
                    resolved_by: *resolved_by,
                },
                None => ResolveError::Rejected(error),
            },
            _ => ResolveError::Rejected(error),
        })
    }

    /// The subscription catch-up for `?after=<after>` (turn-event.md §6): `hello`, `snapshot`,
    /// replayed `event`s from the run boundary, then `live`.
    pub fn catch_up(&self, after: u64) -> Vec<Value> {
        self.core.catch_up(after).unwrap_or_default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use protocol::tunnel::ContentPart;
    use protocol::turn_event::{
        ActorKind, Interrupt, InterruptKind, RunFinished, RunOutcome, RunReason, RunStarted,
        TextMessageEnd, TextMessageStart, UserMessage,
    };
    use serde_json::json;

    const TS: &str = "2026-10-27T22:00:00.000Z";
    /// 2026-10-27T22:00:03Z, before the interrupt below expires.
    const NOW: u64 = 1_793_138_403_000;

    fn env(
        task: bool,
        run: Option<&str>,
        event: impl Into<protocol::turn_event::Event>,
    ) -> Envelope {
        Envelope::new(
            "th_1",
            task.then(|| "tk_1".to_owned()),
            run.map(str::to_owned),
            TS,
            event,
        )
    }

    fn run_env(run: &str, event: impl Into<protocol::turn_event::Event>) -> Envelope {
        env(true, Some(run), event)
    }

    fn user() -> Actor {
        Actor {
            kind: ActorKind::User,
            id: "usr_1".into(),
            name: None,
            device_id: None,
        }
    }

    fn user_message() -> Envelope {
        env(
            true,
            None,
            UserMessage {
                message_id: "msg_1".into(),
                parts: vec![ContentPart::Text { text: "hi".into() }],
                author: user(),
            },
        )
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

    fn interrupt() -> Envelope {
        run_env(
            "run_1",
            Interrupt {
                interrupt_id: "int_1".into(),
                kind: InterruptKind::Approval,
                title: "Allow?".into(),
                message: None,
                expires_at: "2026-10-27T22:10:00.000Z".into(),
                tool_call_id: None,
                options: None,
                response_schema: None,
                url: None,
            },
        )
    }

    #[test]
    fn append_assigns_gap_free_seq_and_refuses_stamped_or_invalid_envelopes() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        assert_eq!(thread.append(user_message(), NOW).map(|e| e.seq), Ok(1));
        assert_eq!(thread.append(started("run_1"), NOW).map(|e| e.seq), Ok(2));
        let start = run_env(
            "run_1",
            TextMessageStart {
                message_id: "m".into(),
                role: Default::default(),
            },
        );
        assert_eq!(thread.append(start, NOW).map(|e| e.seq), Ok(3));
        let mut stamped = run_env(
            "run_1",
            TextMessageEnd {
                message_id: "m".into(),
            },
        );
        stamped.seq = 7;
        assert!(thread.append(stamped, NOW).is_err());
        // §5 is enforced: a RunStarted for a second run while one is open is refused.
        assert!(thread.append(started("run_2"), NOW).is_err());
        assert_eq!(thread.head_seq(), 3);
        assert_eq!(thread.log().len(), 3);
    }

    #[test]
    fn first_answer_wins() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        thread.append(user_message(), NOW).expect("append"); // 1
        thread.append(started("run_1"), NOW).expect("append"); // 2
        thread.append(interrupt(), NOW).expect("append"); // 3
        let paused = run_env(
            "run_1",
            RunFinished {
                outcome: RunOutcome::Interrupt {
                    interrupt_ids: vec!["int_1".into()],
                },
            },
        );
        thread.append(paused, NOW).expect("append"); // 4
        let selected = || ResolvedOutcome::Selected {
            option_id: "allow".into(),
        };
        let resolved = thread
            .resolve_interrupt("int_1", selected(), user(), NOW)
            .expect("first answer accepted");
        assert_eq!(resolved.envelope.seq, 5);
        assert_eq!(resolved.task_id, "tk_1");
        assert_eq!(resolved.envelope.run_id, None);
        assert_eq!(
            thread.resolve_interrupt("int_1", selected(), user(), NOW),
            Err(ResolveError::AlreadyResolved {
                resolved_by: user()
            })
        );
        assert_eq!(
            thread.resolve_interrupt("int_9", selected(), user(), NOW),
            Err(ResolveError::NotFound)
        );
    }

    #[test]
    fn catch_up_replays_from_the_open_run_boundary() {
        let mut thread = ThreadState::new("th_1", "mch_1");
        thread.append(user_message(), NOW).expect("append"); // 1
        thread.append(started("run_1"), NOW).expect("append"); // 2
        thread.append(interrupt(), NOW).expect("append"); // 3

        // Mid-run: replay starts at the run's RunStarted.
        let messages = thread.catch_up(3);
        assert_eq!(
            messages[0],
            json!({"type":"hello","head_seq":3,"replay_from":2})
        );
        assert_eq!(messages[1]["at_seq"], json!(1));
        assert_eq!(messages.len(), 2 + 2 + 1);
        assert_eq!(messages.last(), Some(&json!({"type":"live"})));
    }
}
