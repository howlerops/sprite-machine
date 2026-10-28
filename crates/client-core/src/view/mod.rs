//! View models, built from TurnEvents (turn-event.md §4) with no IO:
//!
//! - [`ThreadView`]: one thread's state (`session`/`usage`/`thread`/`flow` JSON documents),
//!   its tasks and their transcripts, and the host machine's presence. It is a reducer: feed
//!   it the envelopes in `seq` order with [`ThreadView::apply`].
//! - [`transcript`]: a task's ordered items (messages, tool calls, activities, interrupts).
//! - [`inbox`]: the thread list with `idle` / `running` / `needs_you` / `done`.
//! - [`machines`]: machines and their presence, from `Presence` events.

pub mod inbox;
pub mod machines;
pub mod transcript;

use std::collections::BTreeMap;

use protocol::tunnel::Actor;
use protocol::turn_event::{
    Envelope, Event, MachinePresence, RunErrorReason, RunOutcome, StateScope,
};
use serde_json::{Map, Value};

use crate::patch::{self, PatchError};

pub use transcript::{
    ActivityView, InterruptView, Item, LocalAnswer, TextView, ToolStatus, ToolView, Transcript,
    UserView,
};

/// A state or view update that cannot be applied.
#[derive(Debug, thiserror::Error)]
pub enum ViewError {
    #[error(transparent)]
    Patch(#[from] PatchError),
    #[error("state delta for scope {0} arrived before its snapshot")]
    DeltaBeforeSnapshot(String),
}

/// Where a task stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TaskStatus {
    /// `UserMessage` seen, no `RunStarted` yet.
    Starting,
    Running,
    /// Paused on at least one open interrupt.
    NeedsYou,
    Succeeded,
    Failed,
    Cancelled,
}

impl TaskStatus {
    /// `true` once the task has a terminal event.
    pub fn is_finished(self) -> bool {
        matches!(self, Self::Succeeded | Self::Failed | Self::Cancelled)
    }
}

/// One task (one prompt turn) of a thread.
#[derive(Debug, Clone, PartialEq)]
pub struct TaskView {
    pub task_id: String,
    /// The run currently open (or the last one).
    pub run_id: Option<String>,
    started: bool,
    paused: bool,
    finished: Option<TaskStatus>,
    transcript: Transcript,
}

impl TaskView {
    fn new(task_id: &str) -> Self {
        Self {
            task_id: task_id.to_owned(),
            run_id: None,
            started: false,
            paused: false,
            finished: None,
            transcript: Transcript::default(),
        }
    }

    pub fn transcript(&self) -> &Transcript {
        &self.transcript
    }

    pub fn status(&self) -> TaskStatus {
        if let Some(finished) = self.finished {
            return finished;
        }
        if self.paused && self.transcript.interrupts().any(InterruptView::is_open) {
            return TaskStatus::NeedsYou;
        }
        if self.started || self.paused {
            return TaskStatus::Running;
        }
        TaskStatus::Starting
    }

    /// Whether the transcript would refuse `envelope`. Changes nothing.
    fn check(&self, envelope: &Envelope) -> Result<(), ViewError> {
        self.transcript.check(&envelope.event)
    }

    fn apply(&mut self, envelope: &Envelope) -> Result<(), ViewError> {
        // The transcript is the only step that can refuse an event, and it changes nothing when
        // it does. It runs first, so a refused event leaves this task as it was.
        self.transcript.apply(&envelope.event)?;
        match &envelope.event {
            Event::RunStarted(_) => {
                self.run_id = envelope.run_id.clone();
                self.started = true;
                self.paused = false;
            }
            Event::RunFinished(finished) => match &finished.outcome {
                RunOutcome::Success { .. } => self.finished = Some(TaskStatus::Succeeded),
                RunOutcome::Interrupt { .. } => self.paused = true,
            },
            Event::RunError(error) => {
                self.finished = Some(match error.reason {
                    RunErrorReason::Cancelled => TaskStatus::Cancelled,
                    RunErrorReason::Error => TaskStatus::Failed,
                });
            }
            _ => {}
        }
        Ok(())
    }
}

/// The host machine's presence as last reported for a thread.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PresenceView {
    pub state: MachinePresence,
    pub since: String,
}

/// One thread: state documents, tasks, and host presence. Feed it envelopes in `seq` order.
#[derive(Debug, Clone, PartialEq)]
pub struct ThreadView {
    thread_id: String,
    last_seq: u64,
    states: BTreeMap<String, Value>,
    tasks: Vec<TaskView>,
    machine_id: Option<String>,
    presence: Option<PresenceView>,
}

impl ThreadView {
    pub fn new(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            last_seq: 0,
            states: BTreeMap::new(),
            tasks: Vec::new(),
            machine_id: None,
            presence: None,
        }
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The `seq` of the last envelope applied.
    pub fn last_seq(&self) -> u64 {
        self.last_seq
    }

    /// The state document of `scope` (`session`, `usage`, `thread` or `flow`).
    pub fn state(&self, scope: &str) -> Option<&Value> {
        self.states.get(scope)
    }

    /// The thread title, from the `thread` state.
    pub fn title(&self) -> Option<&str> {
        self.state("thread")?.get("title")?.as_str()
    }

    pub fn tasks(&self) -> &[TaskView] {
        &self.tasks
    }

    pub fn task(&self, task_id: &str) -> Option<&TaskView> {
        self.tasks.iter().find(|task| task.task_id == task_id)
    }

    /// The machine hosting this thread, from `RunStarted` or `Presence`.
    pub fn machine_id(&self) -> Option<&str> {
        self.machine_id.as_deref()
    }

    pub fn presence(&self) -> Option<&PresenceView> {
        self.presence.as_ref()
    }

    /// Every interrupt in the thread that nobody has answered yet.
    pub fn open_interrupts(&self) -> Vec<&InterruptView> {
        self.tasks
            .iter()
            .flat_map(|task| task.transcript.interrupts())
            .filter(|interrupt| interrupt.is_open())
            .collect()
    }

    /// Replaces every state document with a `StateSnapshot`-style map (the subscription
    /// `snapshot` message's `states`).
    pub fn install_snapshot(&mut self, states: Map<String, Value>) {
        self.states = states.into_iter().collect();
    }

    /// Whether [`ThreadView::apply`] would refuse `envelope`. Changes nothing, so the caller can
    /// check an event before it persists it.
    pub fn check(&self, envelope: &Envelope) -> Result<(), ViewError> {
        self.staged_delta(envelope)?;
        if let Some(task_id) = &envelope.task_id {
            match self.task(task_id) {
                Some(task) => task.check(envelope)?,
                None => TaskView::new(task_id).check(envelope)?,
            }
        }
        Ok(())
    }

    /// Applies one envelope. The caller guarantees `seq` order without gaps.
    ///
    /// Transactional: if it returns an error, `last_seq` and every other field are unchanged.
    /// Each step that can fail runs before any field is written, and `last_seq` is set last.
    pub fn apply(&mut self, envelope: &Envelope) -> Result<(), ViewError> {
        if matches!(envelope.event, Event::Unknown) {
            // A type from a newer hub: it takes its seq and changes nothing here (§2).
            self.last_seq = envelope.seq;
            return Ok(());
        }
        let staged = self.staged_delta(envelope)?;
        if let Some(task_id) = &envelope.task_id {
            self.apply_to_task(task_id, envelope)?;
        }
        match &envelope.event {
            Event::StateSnapshot(snapshot) => {
                self.states.insert(
                    scope_key(snapshot.scope).to_owned(),
                    Value::Object(snapshot.snapshot.clone()),
                );
            }
            Event::Presence(presence) => {
                self.machine_id = Some(presence.machine_id.clone());
                self.presence = Some(PresenceView {
                    state: presence.state,
                    since: presence.since.clone(),
                });
            }
            Event::RunStarted(started) => {
                if let Some(machine) = &started.machine_id {
                    self.machine_id = Some(machine.clone());
                }
            }
            _ => {}
        }
        if let Some((key, document)) = staged {
            self.states.insert(key.to_owned(), document);
        }
        self.last_seq = envelope.seq;
        Ok(())
    }

    /// The state document `envelope` leaves behind, when it is a `StateDelta`. Changes nothing:
    /// the patch is applied to a copy, and an error means the stored document is untouched.
    fn staged_delta(
        &self,
        envelope: &Envelope,
    ) -> Result<Option<(&'static str, Value)>, ViewError> {
        let Event::StateDelta(delta) = &envelope.event else {
            return Ok(None);
        };
        let key = scope_key(delta.scope);
        let mut document = self
            .states
            .get(key)
            .cloned()
            .ok_or_else(|| ViewError::DeltaBeforeSnapshot(key.to_owned()))?;
        patch::apply(&mut document, &delta.patch)?;
        Ok(Some((key, document)))
    }

    fn apply_to_task(&mut self, task_id: &str, envelope: &Envelope) -> Result<(), ViewError> {
        if let Some(task) = self.tasks.iter_mut().find(|task| task.task_id == task_id) {
            return task.apply(envelope);
        }
        // A new task is added only once its first event has been accepted.
        let mut task = TaskView::new(task_id);
        task.apply(envelope)?;
        self.tasks.push(task);
        Ok(())
    }

    /// Records the `409 already_resolved` winner for an interrupt this device answered.
    pub fn note_conflict(&mut self, interrupt_id: &str, resolved_by: Actor) -> bool {
        self.tasks.iter_mut().any(|task| {
            task.transcript
                .note_conflict(interrupt_id, resolved_by.clone())
        })
    }

    /// Sets (or clears) this device's pending answer for an interrupt.
    pub fn set_local_answer(&mut self, interrupt_id: &str, local: Option<LocalAnswer>) -> bool {
        self.tasks.iter_mut().any(|task| {
            task.transcript
                .set_local_answer(interrupt_id, local.clone())
        })
    }
}

/// The `scope` name used as the key of a state document.
pub fn scope_key(scope: StateScope) -> &'static str {
    match scope {
        StateScope::Session => "session",
        StateScope::Usage => "usage",
        StateScope::Thread => "thread",
        StateScope::Flow => "flow",
    }
}
