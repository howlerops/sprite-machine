//! The session table: one entry per ACP session, with the prompt in flight and the
//! pending tasks queued behind it.
//!
//! A session runs at most one prompt at a time. A prompt sent while another one is in
//! flight waits in the session's queue as a pending task. The queue can be listed with
//! [`SessionTable::pending`] and a task can be removed before it runs. The table does no
//! I/O: the client drives the prompts.

use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::sync::{Mutex, MutexGuard, PoisonError};

use protocol::acp::{ContentBlock, SessionId, StopReason};
use tokio::sync::{oneshot, watch};

use super::client::ClientError;

/// The outcome of one prompt task.
pub type PromptResult = Result<StopReason, ClientError>;

/// Identifies one prompt task. Unique within one client.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct TaskId(pub u64);

impl fmt::Display for TaskId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}", self.0)
    }
}

/// A prompt task waiting behind the one in flight, as listed by [`SessionTable::pending`].
#[derive(Debug, Clone)]
pub struct PendingTask {
    /// The task id, for `AcpClient::remove_pending`.
    pub id: TaskId,
    /// The prompt content the task will send.
    pub prompt: Vec<ContentBlock>,
}

/// A task waiting in a session queue, with the sender for its result.
struct Queued {
    id: TaskId,
    prompt: Vec<ContentBlock>,
    done: oneshot::Sender<PromptResult>,
}

/// What [`SessionTable::enqueue`] did with a new prompt.
pub(crate) struct Enqueued {
    /// The new task's id.
    pub task: TaskId,
    /// `true` when the session was busy, so the task waits as a pending task.
    pub queued: bool,
    /// `true` when no driver runs for the session, so the caller must start one.
    pub start_driver: bool,
    /// Resolves with the task's result, or an error if the task is removed or the
    /// connection closes first.
    pub done: oneshot::Receiver<PromptResult>,
}

/// The next task a driver should run.
pub(crate) struct Started {
    /// The prompt content to send.
    pub prompt: Vec<ContentBlock>,
    /// Where to send the result.
    pub done: oneshot::Sender<PromptResult>,
}

struct Entry {
    /// The task whose prompt is in flight, if any.
    running: Option<TaskId>,
    /// `true` while a driver is running this session's queue.
    driving: bool,
    /// Tasks waiting behind the one in flight, in arrival order.
    queue: VecDeque<Queued>,
    /// Set to `true` by `session/cancel`; reset when the next prompt starts. Permission
    /// and elicitation requests wait on it.
    cancel: watch::Sender<bool>,
}

impl Entry {
    fn new() -> Self {
        let (cancel, _) = watch::channel(false);
        Self {
            running: None,
            driving: false,
            queue: VecDeque::new(),
            cancel,
        }
    }
}

#[derive(Default)]
struct State {
    next_task: u64,
    sessions: HashMap<String, Entry>,
}

/// The session table. Methods lock briefly and never await.
#[derive(Default)]
pub(crate) struct SessionTable {
    state: Mutex<State>,
}

impl SessionTable {
    /// Adds a session. Adding a known session again changes nothing.
    pub(crate) fn insert(&self, session: &SessionId) {
        lock(&self.state)
            .sessions
            .entry(key(session).to_owned())
            .or_insert_with(Entry::new);
    }

    /// Queues a prompt for `session`. The caller starts a driver when `start_driver` is
    /// set.
    pub(crate) fn enqueue(
        &self,
        session: &SessionId,
        prompt: Vec<ContentBlock>,
    ) -> Result<Enqueued, ClientError> {
        let mut guard = lock(&self.state);
        let State {
            next_task,
            sessions,
        } = &mut *guard;
        let entry = sessions
            .get_mut(key(session))
            .ok_or_else(|| ClientError::UnknownSession(key(session).to_owned()))?;
        *next_task += 1;
        let id = TaskId(*next_task);
        let (done, reply) = oneshot::channel();
        let start_driver = !entry.driving;
        entry.driving = true;
        entry.queue.push_back(Queued { id, prompt, done });
        Ok(Enqueued {
            task: id,
            queued: !start_driver,
            start_driver,
            done: reply,
        })
    }

    /// Takes the next queued task and marks it running. Returns `None` and stops the
    /// driver when the queue is empty.
    pub(crate) fn take_next(&self, session: &SessionId) -> Option<Started> {
        let mut state = lock(&self.state);
        let entry = state.sessions.get_mut(key(session))?;
        match entry.queue.pop_front() {
            Some(task) => {
                entry.running = Some(task.id);
                entry.cancel.send_replace(false);
                Some(Started {
                    prompt: task.prompt,
                    done: task.done,
                })
            }
            None => {
                entry.driving = false;
                None
            }
        }
    }

    /// Marks the running task as finished.
    pub(crate) fn finish(&self, session: &SessionId) {
        if let Some(entry) = lock(&self.state).sessions.get_mut(key(session)) {
            entry.running = None;
        }
    }

    /// Removes a pending task. Its result is `Err(ClientError::Removed)`. Returns the
    /// removed task, or `None` when no pending task has that id (it may be running or
    /// finished already).
    pub(crate) fn remove_pending(
        &self,
        session: &SessionId,
        task: TaskId,
    ) -> Result<Option<PendingTask>, ClientError> {
        let mut state = lock(&self.state);
        let entry = state
            .sessions
            .get_mut(key(session))
            .ok_or_else(|| ClientError::UnknownSession(key(session).to_owned()))?;
        let removed = entry
            .queue
            .iter()
            .position(|queued| queued.id == task)
            .and_then(|index| entry.queue.remove(index));
        let Some(queued) = removed else {
            return Ok(None);
        };
        // The caller may have stopped waiting for this task. Then nobody reads the result.
        let _ = queued.done.send(Err(ClientError::Removed));
        Ok(Some(PendingTask {
            id: queued.id,
            prompt: queued.prompt,
        }))
    }

    /// Lists the pending tasks of `session` in the order they will run.
    pub(crate) fn pending(&self, session: &SessionId) -> Result<Vec<PendingTask>, ClientError> {
        let state = lock(&self.state);
        let entry = state
            .sessions
            .get(key(session))
            .ok_or_else(|| ClientError::UnknownSession(key(session).to_owned()))?;
        Ok(entry
            .queue
            .iter()
            .map(|queued| PendingTask {
                id: queued.id,
                prompt: queued.prompt.clone(),
            })
            .collect())
    }

    /// The task whose prompt is in flight in `session`, if any.
    pub(crate) fn running(&self, session: &SessionId) -> Result<Option<TaskId>, ClientError> {
        let state = lock(&self.state);
        let entry = state
            .sessions
            .get(key(session))
            .ok_or_else(|| ClientError::UnknownSession(key(session).to_owned()))?;
        Ok(entry.running)
    }

    /// Flags `session` as cancelled, which answers its waiting permission and elicitation
    /// requests with a cancel. Returns `true` when a prompt is in flight.
    pub(crate) fn request_cancel(&self, session: &SessionId) -> Result<bool, ClientError> {
        let state = lock(&self.state);
        let entry = state
            .sessions
            .get(key(session))
            .ok_or_else(|| ClientError::UnknownSession(key(session).to_owned()))?;
        entry.cancel.send_replace(true);
        Ok(entry.running.is_some())
    }

    /// A receiver that turns `true` when the session is cancelled.
    pub(crate) fn cancel_signal(&self, session: &SessionId) -> Option<watch::Receiver<bool>> {
        lock(&self.state)
            .sessions
            .get(key(session))
            .map(|entry| entry.cancel.subscribe())
    }

    /// Fails every queued task with `ClientError::Closed`. Used when the connection closes.
    pub(crate) fn fail_queued(&self) {
        for entry in lock(&self.state).sessions.values_mut() {
            for queued in entry.queue.drain(..) {
                // The caller may have stopped waiting for this task.
                let _ = queued.done.send(Err(ClientError::Closed));
            }
        }
    }
}

fn key(session: &SessionId) -> &str {
    &session.0
}

/// Locks a std mutex, recovering the guard if another thread panicked while holding it.
/// Every critical section here leaves the state consistent, so the data is still valid.
pub(crate) fn lock<T>(mutex: &Mutex<T>) -> MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(PoisonError::into_inner)
}
