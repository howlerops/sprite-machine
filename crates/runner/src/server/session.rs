//! The seams between one session's bridge and the shared tunnel: a command source fed by the
//! router and an event sink that also tracks which tasks are still running (for `--capacity`).

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use protocol::turn_event::{Envelope, Event, RunOutcome};
use sidecar::bridge::{CommandFailure, CommandSource, EventSink, IncomingCommand, SinkError};
use tokio::sync::mpsc;

use crate::tunnel::TunnelSink;

/// The tasks of one session that have not ended.
pub(super) struct SessionState {
    inner: Mutex<Inner>,
}

impl Default for SessionState {
    fn default() -> Self {
        Self {
            inner: Mutex::new(Inner {
                touched: Instant::now(),
                tasks: HashSet::new(),
                key_task: HashMap::new(),
            }),
        }
    }
}

struct Inner {
    /// When the session last started or ended a task (or was created).
    touched: Instant,
    tasks: HashSet<String>,
    /// Command key → the task it tried to start.
    key_task: HashMap<String, String>,
}

impl SessionState {
    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// A `prompt` or `dispatch` for `task_id` was handed to the bridge.
    pub fn begin(&self, key: &str, task_id: &str) {
        let mut inner = self.lock();
        inner.touched = Instant::now();
        inner.tasks.insert(task_id.to_owned());
        inner.key_task.insert(key.to_owned(), task_id.to_owned());
    }

    /// A task is running or paused.
    pub fn active(&self) -> bool {
        !self.lock().tasks.is_empty()
    }

    /// No task for at least `timeout`.
    pub fn idle_for(&self, timeout: Duration) -> bool {
        let inner = self.lock();
        inner.tasks.is_empty() && inner.touched.elapsed() >= timeout
    }

    fn end_task(&self, task_id: &str) {
        let mut inner = self.lock();
        inner.touched = Instant::now();
        inner.tasks.remove(task_id);
        inner.key_task.retain(|_, task| task != task_id);
    }

    /// The bridge refused the command with `key`: the task it would have started never runs.
    fn fail_key(&self, key: &str) {
        let task = self.lock().key_task.get(key).cloned();
        if let Some(task) = task {
            self.end_task(&task);
        }
    }
}

/// The tunnel sink of one session.
pub(super) struct SessionSink {
    pub inner: TunnelSink,
    pub state: Arc<SessionState>,
}

impl EventSink for SessionSink {
    async fn send(&self, envelopes: Vec<Envelope>) -> Result<(), SinkError> {
        for envelope in &envelopes {
            let ends = match &envelope.event {
                Event::RunError(_) => true,
                Event::RunFinished(finished) => {
                    !matches!(finished.outcome, RunOutcome::Interrupt { .. })
                }
                _ => false,
            };
            if ends && let Some(task_id) = &envelope.task_id {
                self.state.end_task(task_id);
            }
        }
        self.inner.send(envelopes).await
    }

    async fn command_result(&self, failure: CommandFailure) -> Result<(), SinkError> {
        self.state.fail_key(&failure.idempotency_key);
        self.inner.command_result(failure).await
    }
}

/// The commands the router hands to one session's bridge. A processed command is reported to the
/// router, which owns the tunnel's command source and acks it there.
pub(super) struct SessionCommands {
    pub rx: mpsc::UnboundedReceiver<IncomingCommand>,
    pub processed: mpsc::UnboundedSender<(u64, String)>,
}

impl CommandSource for SessionCommands {
    async fn next(&mut self) -> Option<IncomingCommand> {
        self.rx.recv().await
    }

    async fn processed(&mut self, seq: u64, idempotency_key: &str) {
        // Seq 0 is a command the router made up: the hub never sent it, so there is nothing to ack.
        if seq != 0 {
            let _ = self.processed.send((seq, idempotency_key.to_owned()));
        }
    }
}
