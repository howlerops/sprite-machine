//! Down-stream commands: the `command` field of a `command` frame (§3.5).

use serde::{Deserialize, Serialize};

use super::types::{Actor, ContentPart, Outcome, Workspace};

/// A command from the hub to the runner, tagged by `kind`.
///
/// Unknown kinds fail to parse. The runner must answer them with
/// `command_result{ok: false, code: "unsupported"}` (§3.5), so it inspects the raw `kind`
/// before deserializing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Command {
    /// Start a task in an existing thread.
    Prompt {
        thread_id: String,
        task_id: String,
        message_id: String,
        parts: Vec<ContentPart>,
        author: Actor,
    },
    /// The accepted answer to an interrupt.
    Approval {
        thread_id: String,
        task_id: String,
        interrupt_id: String,
        outcome: Outcome,
        resolved_by: Actor,
    },
    /// Start a new thread on this machine.
    Dispatch {
        thread_id: String,
        task_id: String,
        agent: String,
        workspace: Workspace,
        permission_mode: String,
        message_id: String,
        parts: Vec<ContentPart>,
        author: Actor,
    },
    /// Cancel the running task.
    Cancel { thread_id: String, task_id: String },
}
