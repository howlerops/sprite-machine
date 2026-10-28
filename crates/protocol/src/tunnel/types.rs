//! Small value types shared by tunnel frames and commands (§3.4, §3.5).

use serde::{Deserialize, Serialize};

/// Who produced an action: a user on a device, a policy, the system, or an agent.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActorKind {
    User,
    Policy,
    System,
    Agent,
}

/// An actor as carried in `author` and `resolved_by` (same shape as turn-event.md §4.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Actor {
    pub kind: ActorKind,
    pub id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub device_id: Option<String>,
}

/// One message part: text, or a file the hub has made reachable by https URL.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum ContentPart {
    Text {
        text: String,
    },
    File {
        name: String,
        mime_type: String,
        url: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        size: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sha256: Option<String>,
    },
}

/// The answer to an interrupt, carried by an `approval` command (§3.5, SPEC-002a): the
/// `InterruptResolved.outcome` union of turn-event.md §4.5 minus `expired` (expiry is decided
/// by the runner and never sent down).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    /// An option the user picked (ACP `RequestPermissionOutcome::Selected`).
    Selected { option_id: String },
    /// A filled form (elicitation `accept`).
    Submitted { data: serde_json::Value },
    /// A form or auth interrupt that was declined (elicitation `decline`).
    Declined,
    /// Cancelled (ACP `Cancelled` / elicitation `cancel`).
    Cancelled,
}

/// Workspace reference. `thread_open` carries `branch`; `dispatch` carries `spawn`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Workspace {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub branch: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spawn: Option<String>,
}
