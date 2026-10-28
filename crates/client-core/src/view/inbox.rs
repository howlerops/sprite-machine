//! The inbox: one row per thread, with the status the thread list shows
//! (`client-api.md` §5.2: `idle | running | needs_you | done`).

use std::cmp::Reverse;

use super::{TaskStatus, ThreadView};

/// A thread's status in the list.
///
/// A thread is `needs_you` while its current task waits on an open interrupt, `running` while
/// a task is starting or running, `done` once its latest task has finished, and `idle` while
/// it has no task. Declared in display-priority order, so sorting puts `needs_you` first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum InboxStatus {
    NeedsYou,
    Running,
    Idle,
    Done,
}

/// One inbox row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InboxRow {
    pub thread_id: String,
    pub title: Option<String>,
    pub status: InboxStatus,
    pub machine_id: Option<String>,
    pub last_seq: u64,
    /// Open interrupts across the thread: the number of answers this user owes.
    pub open_interrupts: usize,
}

/// The status of one thread.
pub fn thread_status(thread: &ThreadView) -> InboxStatus {
    match thread.tasks().last().map(|task| task.status()) {
        None => InboxStatus::Idle,
        Some(TaskStatus::NeedsYou) => InboxStatus::NeedsYou,
        Some(TaskStatus::Starting | TaskStatus::Running) => InboxStatus::Running,
        Some(_) => InboxStatus::Done,
    }
}

/// Rows for `threads`, sorted by status, then most recently changed first, then thread id.
pub fn inbox<'a>(threads: impl IntoIterator<Item = &'a ThreadView>) -> Vec<InboxRow> {
    let mut rows: Vec<InboxRow> = threads
        .into_iter()
        .map(|thread| InboxRow {
            thread_id: thread.thread_id().to_owned(),
            title: thread.title().map(str::to_owned),
            status: thread_status(thread),
            machine_id: thread.machine_id().map(str::to_owned),
            last_seq: thread.last_seq(),
            open_interrupts: thread.open_interrupts().len(),
        })
        .collect();
    rows.sort_by(|a, b| {
        (a.status, Reverse(a.last_seq), &a.thread_id).cmp(&(
            b.status,
            Reverse(b.last_seq),
            &b.thread_id,
        ))
    });
    rows
}
