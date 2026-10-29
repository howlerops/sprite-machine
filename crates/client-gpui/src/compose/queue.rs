//! The prompts waiting behind the running task (the `thread` state's `pending_tasks`,
//! turn-event.md §4.6) and the task Stop would cancel.

use client_core::ThreadView;
use client_core::outbox::Intent;

/// One queued message, shown as a chip.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedChip {
    pub task_id: String,
    pub preview: String,
}

/// The queued messages, oldest first.
pub fn queued_chips(thread: &ThreadView) -> Vec<QueuedChip> {
    thread
        .state("thread")
        .and_then(|state| state.get("pending_tasks"))
        .and_then(|pending| pending.as_array())
        .into_iter()
        .flatten()
        .filter_map(|entry| {
            let task_id = entry.get("task_id")?.as_str()?.to_owned();
            let preview = entry
                .get("preview")
                .and_then(|preview| preview.as_str())
                .unwrap_or_default()
                .to_owned();
            Some(QueuedChip { task_id, preview })
        })
        .collect()
}

/// The task that is running (or waiting on an answer), if any.
pub fn running_task(thread: &ThreadView) -> Option<&str> {
    thread
        .tasks()
        .iter()
        .rev()
        .find(|task| !task.status().is_finished())
        .map(|task| task.task_id.as_str())
}

/// The cancel for the running task.
pub fn stop_intent(thread: &ThreadView) -> Option<Intent> {
    Some(Intent::Cancel {
        thread_id: thread.thread_id().to_owned(),
        task_id: running_task(thread)?.to_owned(),
    })
}

/// The intent that drops a queued message.
pub fn remove_request(thread: &ThreadView, task_id: &str) -> Option<Intent> {
    queued_chips(thread)
        .iter()
        .any(|chip| chip.task_id == task_id)
        .then(|| Intent::RemovePending {
            thread_id: thread.thread_id().to_owned(),
            task_id: task_id.to_owned(),
        })
}

/// The cancel Stop sends: the running task, else (after a `409 task_started` on a removal) the
/// task that started before the removal arrived, unless the thread already shows it finished.
pub fn stoppable_intent(thread: &ThreadView, started: &[String]) -> Option<Intent> {
    if let Some(intent) = stop_intent(thread) {
        return Some(intent);
    }
    let task_id = started.iter().find(|task_id| {
        !thread
            .tasks()
            .iter()
            .any(|task| &task.task_id == *task_id && task.status().is_finished())
    })?;
    Some(Intent::Cancel {
        thread_id: thread.thread_id().to_owned(),
        task_id: task_id.clone(),
    })
}
