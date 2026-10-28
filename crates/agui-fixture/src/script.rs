//! Scenario names, the pre-generated script (steps the server plays in order) and the builder
//! that the scenario functions use to write envelopes with deterministic `ts` values.

use std::fmt;
use std::str::FromStr;

use protocol::tunnel::{Actor, ActorKind, ContentPart};
use protocol::turn_event::{
    Envelope, Event, InterruptResolved, RawPayload, RawSource, ResolvedOutcome, RunFinished,
    RunOutcome, RunReason, RunStarted, ThreadSeq, UserMessage, format_timestamp,
};

use crate::scenarios;

/// First `ts` of every scenario (2026-10-27T22:00:00.000Z).
pub(crate) const BASE_MS: u64 = 1_793_138_400_000;
/// Producer time between two consecutive scripted envelopes.
const TICK_MS: u64 = 25;
/// How long an approval stays open (plan §4.1 default TTL).
pub(crate) const APPROVAL_TTL_MS: u64 = 600_000;
/// The option the default (offline) answer selects.
pub const APPROVE_OPTION: &str = "allow";

/// One of the fixture's canned threads.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Scenario {
    /// About 10,000 events over a few hundred tasks: messages, reasoning, tool calls, plans,
    /// usage deltas, checkpoints.
    Transcript10k,
    /// One task that writes a 5,000-line file as a chunked `Activity{diff}`.
    Diff5k,
    /// One task that streams terminal output in `append` mode, then exits.
    Terminal,
    /// A run that pauses on an approval `Interrupt`; the resume run starts after `POST`.
    Approval,
}

impl Scenario {
    /// Every scenario, in documentation order.
    pub const ALL: [Scenario; 4] = [
        Scenario::Transcript10k,
        Scenario::Diff5k,
        Scenario::Terminal,
        Scenario::Approval,
    ];

    /// The name used on the command line.
    pub fn name(self) -> &'static str {
        match self {
            Scenario::Transcript10k => "transcript-10k",
            Scenario::Diff5k => "diff-5k",
            Scenario::Terminal => "terminal",
            Scenario::Approval => "approval",
        }
    }

    /// The full script of this scenario on `thread_id`.
    pub fn script(self, thread_id: &str) -> Script {
        match self {
            Scenario::Transcript10k => scenarios::transcript(thread_id),
            Scenario::Diff5k => scenarios::diff(thread_id),
            Scenario::Terminal => scenarios::terminal(thread_id),
            Scenario::Approval => scenarios::approval(thread_id),
        }
    }
}

impl fmt::Display for Scenario {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

impl FromStr for Scenario {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        Scenario::ALL
            .into_iter()
            .find(|scenario| scenario.name() == s)
            .ok_or_else(|| {
                let names: Vec<&str> = Scenario::ALL.iter().map(|s| s.name()).collect();
                format!(
                    "unknown scenario `{s}`; expected one of {}",
                    names.join(", ")
                )
            })
    }
}

/// One thing the server does, in order.
#[derive(Debug, Clone)]
pub enum Step {
    /// Append this envelope (`seq: 0`; the server stamps it).
    Emit(Box<Envelope>),
    /// Wait until the interrupt is answered over HTTP, then append its `InterruptResolved`
    /// (task scope) with the recorded answer. The run continues after this step.
    AwaitAnswer {
        task_id: String,
        interrupt_id: String,
        /// `ts` of the `InterruptResolved` envelope.
        ts: String,
    },
}

/// A scenario's steps, in playback order.
pub type Script = Vec<Step>;

/// The user that answers the fixture's interrupts when a request names no `resolved_by`.
pub fn default_actor() -> Actor {
    Actor {
        kind: ActorKind::User,
        id: "usr_fixture".to_owned(),
        name: Some("Fixture user".to_owned()),
        device_id: Some("dev_fixture".to_owned()),
    }
}

/// The whole log of a scenario, stamped with `seq` 1..=N, as the server would store it when
/// every interrupt is answered with [`APPROVE_OPTION`] by [`default_actor`].
pub fn complete_log(scenario: Scenario, thread_id: &str) -> anyhow::Result<Vec<Envelope>> {
    let mut seq = ThreadSeq::new();
    let mut log = Vec::new();
    for step in scenario.script(thread_id) {
        let envelope = match step {
            Step::Emit(envelope) => *envelope,
            Step::AwaitAnswer {
                task_id,
                interrupt_id,
                ts,
            } => resolution_envelope(
                thread_id,
                &task_id,
                &interrupt_id,
                ResolvedOutcome::Selected {
                    option_id: APPROVE_OPTION.to_owned(),
                },
                default_actor(),
                ts,
            ),
        };
        log.push(seq.stamp(envelope)?);
    }
    Ok(log)
}

/// The `InterruptResolved` envelope for an answered interrupt (task scope).
pub(crate) fn resolution_envelope(
    thread_id: &str,
    task_id: &str,
    interrupt_id: &str,
    outcome: ResolvedOutcome,
    resolved_by: Actor,
    ts: String,
) -> Envelope {
    Envelope::new(
        thread_id,
        Some(task_id.to_owned()),
        None,
        ts,
        InterruptResolved {
            interrupt_id: interrupt_id.to_owned(),
            outcome,
            resolved_by,
        },
    )
}

/// Appends scripted envelopes with a deterministic clock.
pub(crate) struct Builder {
    thread_id: String,
    clock_ms: u64,
    steps: Script,
    emitted: usize,
}

impl Builder {
    pub(crate) fn new(thread_id: &str) -> Self {
        Self {
            thread_id: thread_id.to_owned(),
            clock_ms: BASE_MS,
            steps: Vec::new(),
            emitted: 0,
        }
    }

    /// Number of envelopes emitted so far.
    pub(crate) fn emitted(&self) -> usize {
        self.emitted
    }

    /// A thread-scoped event (both ids null).
    pub(crate) fn thread_event(&mut self, event: impl Into<Event>) {
        self.push(None, None, event.into(), None);
    }

    /// A task-scoped event (`run_id` null).
    pub(crate) fn task_event(&mut self, task: &str, event: impl Into<Event>) {
        self.push(Some(task), None, event.into(), None);
    }

    /// A run-scoped event.
    pub(crate) fn run_event(&mut self, task: &str, run: &str, event: impl Into<Event>) {
        self.push(Some(task), Some(run), event.into(), None);
    }

    /// A run-scoped event carrying the ACP payload it was mapped from (`raw`).
    pub(crate) fn run_event_raw(
        &mut self,
        task: &str,
        run: &str,
        event: impl Into<Event>,
        payload: serde_json::Value,
    ) {
        let raw = RawPayload {
            source: RawSource::Acp,
            payload,
        };
        self.push(Some(task), Some(run), event.into(), Some(raw));
    }

    /// Adds a step that waits for the answer to `interrupt_id` (emitted after the run's
    /// `RunFinished{interrupt}`), then appends its `InterruptResolved`.
    pub(crate) fn await_answer(&mut self, task: &str, interrupt_id: &str) {
        let ts = self.timestamp();
        self.steps.push(Step::AwaitAnswer {
            task_id: task.to_owned(),
            interrupt_id: interrupt_id.to_owned(),
            ts,
        });
    }

    /// A `RunStarted` with the given reason and the fixture's agent and machine.
    pub(crate) fn run_started(&mut self, task: &str, run: &str, reason: RunReason) {
        self.run_event(
            task,
            run,
            RunStarted {
                reason,
                parent_run_id: None,
                agent_id: Some("agt_fixture".to_owned()),
                machine_id: Some("mch_fixture".to_owned()),
            },
        );
    }

    /// `RunFinished{success}` with `end_turn`.
    pub(crate) fn run_success(&mut self, task: &str, run: &str) {
        self.run_event(
            task,
            run,
            RunFinished {
                outcome: RunOutcome::Success {
                    stop_reason: Some(protocol::turn_event::StopReason::EndTurn),
                },
            },
        );
    }

    /// `ts` a given number of milliseconds after the current script position.
    pub(crate) fn timestamp_after(&self, offset_ms: u64) -> String {
        format_timestamp(self.clock_ms + offset_ms)
    }

    /// Current script `ts`.
    fn timestamp(&mut self) -> String {
        let ts = format_timestamp(self.clock_ms);
        self.clock_ms += TICK_MS;
        ts
    }

    fn push(
        &mut self,
        task: Option<&str>,
        run: Option<&str>,
        event: Event,
        raw: Option<RawPayload>,
    ) {
        let ts = self.timestamp();
        let mut envelope = Envelope::new(
            self.thread_id.clone(),
            task.map(str::to_owned),
            run.map(str::to_owned),
            ts,
            event,
        );
        envelope.raw = raw;
        self.steps.push(Step::Emit(Box::new(envelope)));
        self.emitted += 1;
    }

    pub(crate) fn into_script(self) -> Script {
        self.steps
    }
}

/// A user message from [`default_actor`] that opens a task.
pub(crate) fn user_message(message_id: &str, text: &str) -> UserMessage {
    UserMessage {
        message_id: message_id.to_owned(),
        parts: vec![ContentPart::Text {
            text: text.to_owned(),
        }],
        author: default_actor(),
    }
}
