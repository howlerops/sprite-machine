//! Down-stream commands: the `command` field of a `command` frame (§3.5).

use serde::de::Error as _;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::frame::{CommandErrorCode, Frame};
use super::types::{Actor, ContentPart, Outcome, Workspace};

/// A command from the hub to the runner, tagged by `kind`.
///
/// Unknown kinds fail to parse. The runner must answer them with
/// `command_result{ok: false, code: "unsupported"}` (§3.5), so it parses frames with
/// [`parse_command_frame`], which reads the raw `kind` before typed parsing.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
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
    /// Change the thread's ACP session mode and/or one config option (§3.5). At least one of
    /// `mode_id` and `config` is set; the runner checks both against what the session advertised.
    SessionConfig {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<SessionConfigChange>,
    },
}

/// The `config` of a `session_config` command: the id of a config option and its new value.
///
/// `value` is a string (a select option's value id) or a boolean (a boolean option).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SessionConfigChange {
    pub id: String,
    pub value: Value,
}

/// The `kind` values this build of the protocol understands (§3.5).
pub const KNOWN_COMMAND_KINDS: [&str; 5] =
    ["prompt", "approval", "dispatch", "cancel", "session_config"];

/// Result of parsing a `command` frame with [`parse_command_frame`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ParsedCommand {
    /// A known `kind`, fully and strictly parsed. Always a [`Frame::Command`].
    Typed(Box<Frame>),
    /// An unknown `kind`. The envelope was validated strictly; the command body is not checked,
    /// since its shape is unknown to this build. The runner replies with
    /// [`ParsedCommand::unsupported_result`] (§3.5).
    Unsupported {
        seq: u64,
        idempotency_key: String,
        issued_at: String,
        kind: String,
    },
}

impl ParsedCommand {
    /// For [`ParsedCommand::Unsupported`], the `command_result{ok:false, code:"unsupported"}`
    /// frame the runner sends (§3.5, §3.6). `up_seq` and `event_id` are the runner's own up
    /// stream values, assigned by the caller. Returns `None` for a typed command.
    pub fn unsupported_result(&self, up_seq: u64, event_id: &str) -> Option<Frame> {
        match self {
            Self::Typed(_) => None,
            Self::Unsupported {
                idempotency_key,
                kind,
                ..
            } => Some(Frame::CommandResult {
                seq: up_seq,
                event_id: event_id.to_owned(),
                idempotency_key: idempotency_key.clone(),
                ok: false,
                code: CommandErrorCode::Unsupported,
                message: format!("unsupported command kind `{kind}`"),
            }),
        }
    }
}

/// Parses a `command` frame (§3.5) without losing the `kind` of an unknown command.
///
/// - Known `kind`: the whole frame is parsed strictly as a [`Frame`]. Extra fields and malformed
///   bodies are errors.
/// - Unknown `kind`: the envelope is validated strictly (`t`, `seq`, `idempotency_key`,
///   `issued_at` and an object `command`; no other top-level fields), and the result is
///   [`ParsedCommand::Unsupported`].
///
/// Errors when the input is not a `command` frame or has no string `command.kind`.
pub fn parse_command_frame(input: &str) -> Result<ParsedCommand, serde_json::Error> {
    /// Envelope of a `command` frame whose `kind` this build does not know.
    #[derive(Deserialize)]
    #[serde(deny_unknown_fields)]
    struct Envelope {
        t: String,
        seq: u64,
        idempotency_key: String,
        issued_at: String,
        command: Value,
    }

    let value: Value = serde_json::from_str(input)?;
    if value.get("t").and_then(Value::as_str) != Some("command") {
        return Err(serde_json::Error::custom("not a `command` frame"));
    }
    let kind = value
        .get("command")
        .and_then(|c| c.get("kind"))
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| serde_json::Error::custom("`command.kind` must be a string"))?;

    if KNOWN_COMMAND_KINDS.contains(&kind.as_str()) {
        return match serde_json::from_value::<Frame>(value)? {
            frame @ Frame::Command { .. } => Ok(ParsedCommand::Typed(Box::new(frame))),
            _ => Err(serde_json::Error::custom("not a `command` frame")),
        };
    }

    let envelope: Envelope = serde_json::from_value(value)?;
    if envelope.t != "command" || !envelope.command.is_object() {
        return Err(serde_json::Error::custom("malformed `command` frame"));
    }
    Ok(ParsedCommand::Unsupported {
        seq: envelope.seq,
        idempotency_key: envelope.idempotency_key,
        issued_at: envelope.issued_at,
        kind,
    })
}
