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

/// The answer to an interrupt: an option the user picked, or cancelled (ACP
/// `RequestPermissionOutcome`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case", deny_unknown_fields)]
pub enum Outcome {
    Selected { option_id: String },
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
