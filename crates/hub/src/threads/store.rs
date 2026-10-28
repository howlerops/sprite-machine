//! The persistence interface of one thread ([`ThreadStore`]) and an in-memory implementation
//! ([`MemoryStore`]) for native tests and the tracer. The Durable Object implements the same
//! trait over its SQLite database (`sqlite.rs`, wasm32 only).
//!
//! The trait is deliberately dumb: no business rules, only the reads and writes the
//! [`ThreadCore`](super::ThreadCore) needs. Every method is synchronous because Durable Object
//! SQL is; the DO therefore applies one request's writes without yielding, which makes them
//! atomic.

use protocol::tunnel::{Actor, Command, ContentPart};
use protocol::turn_event::{Envelope, InterruptKind, ResolvedOutcome, StateScope};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A storage failure (becomes HTTP 500).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StoreError(pub String);

impl StoreError {
    pub fn new(message: impl std::fmt::Display) -> Self {
        Self(message.to_string())
    }
}

impl std::fmt::Display for StoreError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

impl std::error::Error for StoreError {}

pub type StoreResult<T> = Result<T, StoreError>;

/// Where a task is in its life.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskStatus {
    /// Queued behind a running task; no `UserMessage` yet.
    Pending,
    /// Started: `UserMessage` emitted, no terminal event yet (paused on an interrupt counts).
    Running,
    /// Ended (`RunFinished{success}`, `RunError`) or dropped from the queue by a cancel.
    Finished,
}

impl TaskStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Running => "running",
            Self::Finished => "finished",
        }
    }
}

/// One task (prompt turn) the thread knows about.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct TaskRecord {
    pub task_id: String,
    pub status: TaskStatus,
    pub message_id: String,
    pub parts: Vec<ContentPart>,
    pub author: Actor,
    /// First characters of the first text part.
    pub preview: String,
    pub queued_at: String,
    /// The client's `Idempotency-Key`, reused as the tunnel command's key.
    pub idempotency_key: String,
    /// A `RunStarted` was seen for this task.
    pub has_run: bool,
    pub cancel_requested: bool,
}

/// The `seq` span of one run: from its `RunStarted` to its terminal event (if any yet).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunRecord {
    pub run_id: String,
    pub task_id: String,
    pub start_seq: u64,
    pub end_seq: Option<u64>,
}

/// Who answered an interrupt and how.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Resolution {
    pub resolved_by: Actor,
    pub outcome: ResolvedOutcome,
}

/// One interrupt of this thread.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptRecord {
    pub interrupt_id: String,
    pub task_id: String,
    pub kind: InterruptKind,
    /// `option_id`s of an approval; empty when the interrupt lists none.
    pub option_ids: Vec<String>,
    pub expires_at: String,
    pub resolved: Option<Resolution>,
}

/// A command waiting for the Machine DO (see `ThreadCore::pending_commands`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CommandRecord {
    /// Monotonic per thread; the Machine DO acknowledges by id.
    pub id: u64,
    /// Travels down the tunnel as the command's `idempotency_key` (tunnel.md §3.5).
    pub idempotency_key: String,
    pub issued_at: String,
    pub command: Command,
}

/// A stored `(key, user)` answer (client-api.md §3).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct IdempotencyRecord {
    pub user_id: String,
    pub key: String,
    pub request_hash: String,
    pub status: u16,
    pub body: Value,
    pub created_ms: u64,
}

/// What [`ThreadCore`](super::ThreadCore) needs from storage.
pub trait ThreadStore {
    // ---- event log ----
    /// The last stored `seq` (0 when empty).
    fn head_seq(&self) -> StoreResult<u64>;
    /// Stores a stamped envelope. The caller guarantees `seq == head_seq + 1`.
    fn append_event(&mut self, envelope: &Envelope) -> StoreResult<()>;
    /// Envelopes with `from <= seq <= to`, in order.
    fn read_range(&self, from: u64, to: u64) -> StoreResult<Vec<Envelope>>;

    // ---- runs ----
    fn put_run(&mut self, run: &RunRecord) -> StoreResult<()>;
    fn get_run(&self, run_id: &str) -> StoreResult<Option<RunRecord>>;
    /// The smallest `start_seq` of a run that was open at `after` (`start_seq <= after` and not
    /// ended at or before `after`).
    fn run_boundary_at(&self, after: u64) -> StoreResult<Option<u64>>;

    // ---- state per scope, as of each change ----
    fn put_state(&mut self, scope: StateScope, seq: u64, state: &Value) -> StoreResult<()>;
    /// The state of `scope` as of `seq` (the latest change at or before it).
    fn state_at(&self, scope: StateScope, seq: u64) -> StoreResult<Option<Value>>;

    // ---- tasks (the pending queue is the `Pending` ones, oldest first) ----
    /// Inserts or replaces by `task_id`; a replaced task keeps its queue position.
    fn put_task(&mut self, task: &TaskRecord) -> StoreResult<()>;
    fn get_task(&self, task_id: &str) -> StoreResult<Option<TaskRecord>>;
    /// Tasks with this status, in first-insertion order.
    fn tasks_with_status(&self, status: TaskStatus) -> StoreResult<Vec<TaskRecord>>;

    // ---- interrupts ----
    fn put_interrupt(&mut self, interrupt: &InterruptRecord) -> StoreResult<()>;
    fn get_interrupt(&self, interrupt_id: &str) -> StoreResult<Option<InterruptRecord>>;
    /// Unresolved interrupts of a task, oldest first.
    fn open_interrupts(&self, task_id: &str) -> StoreResult<Vec<InterruptRecord>>;

    // ---- commands outbox ----
    /// Queues a command and returns its id.
    fn queue_command(
        &mut self,
        idempotency_key: &str,
        issued_at: &str,
        command: &Command,
    ) -> StoreResult<u64>;
    /// Commands not yet acknowledged, oldest first.
    fn unacked_commands(&self, limit: usize) -> StoreResult<Vec<CommandRecord>>;
    /// Marks commands delivered; returns how many were still unacknowledged.
    fn ack_commands(&mut self, ids: &[u64]) -> StoreResult<usize>;
    /// Deletes the unacknowledged `prompt` command of a task; `true` when there was one.
    fn retract_prompt_command(&mut self, task_id: &str) -> StoreResult<bool>;

    // ---- idempotency ----
    fn get_idempotency(&self, user_id: &str, key: &str) -> StoreResult<Option<IdempotencyRecord>>;
    fn put_idempotency(&mut self, record: &IdempotencyRecord) -> StoreResult<()>;
    /// Deletes records created before `older_than_ms`.
    fn prune_idempotency(&mut self, older_than_ms: u64) -> StoreResult<()>;

    // ---- small stuff ----
    fn get_meta(&self, key: &str) -> StoreResult<Option<Value>>;
    fn put_meta(&mut self, key: &str, value: &Value) -> StoreResult<()>;
    /// A per-thread counter that only goes up (id generation).
    fn next_counter(&mut self) -> StoreResult<u64>;
}

/// [`ThreadStore`] in memory.
#[derive(Debug, Default, Clone)]
pub struct MemoryStore {
    log: Vec<Envelope>,
    runs: Vec<RunRecord>,
    states: Vec<(StateScope, u64, Value)>,
    tasks: Vec<TaskRecord>,
    interrupts: Vec<InterruptRecord>,
    commands: Vec<(CommandRecord, bool)>,
    next_command: u64,
    idempotency: Vec<IdempotencyRecord>,
    meta: Vec<(String, Value)>,
    counter: u64,
}

impl MemoryStore {
    pub fn new() -> Self {
        Self::default()
    }
}

impl ThreadStore for MemoryStore {
    fn head_seq(&self) -> StoreResult<u64> {
        Ok(self.log.last().map_or(0, |envelope| envelope.seq))
    }

    fn append_event(&mut self, envelope: &Envelope) -> StoreResult<()> {
        self.log.push(envelope.clone());
        Ok(())
    }

    fn read_range(&self, from: u64, to: u64) -> StoreResult<Vec<Envelope>> {
        Ok(self
            .log
            .iter()
            .filter(|envelope| envelope.seq >= from && envelope.seq <= to)
            .cloned()
            .collect())
    }

    fn put_run(&mut self, run: &RunRecord) -> StoreResult<()> {
        match self.runs.iter_mut().find(|r| r.run_id == run.run_id) {
            Some(existing) => *existing = run.clone(),
            None => self.runs.push(run.clone()),
        }
        Ok(())
    }

    fn get_run(&self, run_id: &str) -> StoreResult<Option<RunRecord>> {
        Ok(self.runs.iter().find(|r| r.run_id == run_id).cloned())
    }

    fn run_boundary_at(&self, after: u64) -> StoreResult<Option<u64>> {
        Ok(self
            .runs
            .iter()
            .filter(|run| run.start_seq <= after && run.end_seq.is_none_or(|end| end > after))
            .map(|run| run.start_seq)
            .min())
    }

    fn put_state(&mut self, scope: StateScope, seq: u64, state: &Value) -> StoreResult<()> {
        self.states.push((scope, seq, state.clone()));
        Ok(())
    }

    fn state_at(&self, scope: StateScope, seq: u64) -> StoreResult<Option<Value>> {
        Ok(self
            .states
            .iter()
            .rev()
            .find(|(s, at, _)| *s == scope && *at <= seq)
            .map(|(_, _, state)| state.clone()))
    }

    fn put_task(&mut self, task: &TaskRecord) -> StoreResult<()> {
        match self.tasks.iter_mut().find(|t| t.task_id == task.task_id) {
            Some(existing) => *existing = task.clone(),
            None => self.tasks.push(task.clone()),
        }
        Ok(())
    }

    fn get_task(&self, task_id: &str) -> StoreResult<Option<TaskRecord>> {
        Ok(self.tasks.iter().find(|t| t.task_id == task_id).cloned())
    }

    fn tasks_with_status(&self, status: TaskStatus) -> StoreResult<Vec<TaskRecord>> {
        Ok(self
            .tasks
            .iter()
            .filter(|t| t.status == status)
            .cloned()
            .collect())
    }

    fn put_interrupt(&mut self, interrupt: &InterruptRecord) -> StoreResult<()> {
        match self
            .interrupts
            .iter_mut()
            .find(|i| i.interrupt_id == interrupt.interrupt_id)
        {
            Some(existing) => *existing = interrupt.clone(),
            None => self.interrupts.push(interrupt.clone()),
        }
        Ok(())
    }

    fn get_interrupt(&self, interrupt_id: &str) -> StoreResult<Option<InterruptRecord>> {
        Ok(self
            .interrupts
            .iter()
            .find(|i| i.interrupt_id == interrupt_id)
            .cloned())
    }

    fn open_interrupts(&self, task_id: &str) -> StoreResult<Vec<InterruptRecord>> {
        Ok(self
            .interrupts
            .iter()
            .filter(|i| i.task_id == task_id && i.resolved.is_none())
            .cloned()
            .collect())
    }

    fn queue_command(
        &mut self,
        idempotency_key: &str,
        issued_at: &str,
        command: &Command,
    ) -> StoreResult<u64> {
        self.next_command += 1;
        let record = CommandRecord {
            id: self.next_command,
            idempotency_key: idempotency_key.to_owned(),
            issued_at: issued_at.to_owned(),
            command: command.clone(),
        };
        self.commands.push((record, false));
        Ok(self.next_command)
    }

    fn unacked_commands(&self, limit: usize) -> StoreResult<Vec<CommandRecord>> {
        Ok(self
            .commands
            .iter()
            .filter(|(_, acked)| !acked)
            .take(limit)
            .map(|(record, _)| record.clone())
            .collect())
    }

    fn ack_commands(&mut self, ids: &[u64]) -> StoreResult<usize> {
        let mut changed = 0;
        for (record, acked) in &mut self.commands {
            if !*acked && ids.contains(&record.id) {
                *acked = true;
                changed += 1;
            }
        }
        Ok(changed)
    }

    fn retract_prompt_command(&mut self, task_id: &str) -> StoreResult<bool> {
        let before = self.commands.len();
        self.commands.retain(|(record, acked)| {
            *acked || !matches!(&record.command, Command::Prompt { task_id: t, .. } if t == task_id)
        });
        Ok(self.commands.len() != before)
    }

    fn get_idempotency(&self, user_id: &str, key: &str) -> StoreResult<Option<IdempotencyRecord>> {
        Ok(self
            .idempotency
            .iter()
            .find(|r| r.user_id == user_id && r.key == key)
            .cloned())
    }

    fn put_idempotency(&mut self, record: &IdempotencyRecord) -> StoreResult<()> {
        self.idempotency
            .retain(|r| !(r.user_id == record.user_id && r.key == record.key));
        self.idempotency.push(record.clone());
        Ok(())
    }

    fn prune_idempotency(&mut self, older_than_ms: u64) -> StoreResult<()> {
        self.idempotency.retain(|r| r.created_ms >= older_than_ms);
        Ok(())
    }

    fn get_meta(&self, key: &str) -> StoreResult<Option<Value>> {
        Ok(self
            .meta
            .iter()
            .find(|(k, _)| k == key)
            .map(|(_, v)| v.clone()))
    }

    fn put_meta(&mut self, key: &str, value: &Value) -> StoreResult<()> {
        match self.meta.iter_mut().find(|(k, _)| k == key) {
            Some((_, existing)) => *existing = value.clone(),
            None => self.meta.push((key.to_owned(), value.clone())),
        }
        Ok(())
    }

    fn next_counter(&mut self) -> StoreResult<u64> {
        self.counter += 1;
        Ok(self.counter)
    }
}
