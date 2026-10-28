//! Frame types for the tunnel (§3). One enum, tagged by `t`.

use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::command::Command;
use super::types::Workspace;

/// Every tunnel frame. Sequenced frames (`event`, `thread_open`, `command_result` up;
/// `command` down) carry `seq`; the rest do not.
///
/// `ping` and `pong` serialize to exactly `{"t":"ping"}` and `{"t":"pong"}` (§3.7), so the
/// hub can answer them with `setWebSocketAutoResponse` without waking.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case", deny_unknown_fields)]
pub enum Frame {
    /// Runner → hub, first frame after connect (§3.1).
    Hello {
        proto: u32,
        machine_id: String,
        runner_version: String,
        /// Token from the last `welcome`, or `null` on first connect / after state loss.
        resume_token: Option<String>,
        /// Highest command `seq` the runner has durably processed (0 if none).
        last_down_seq: u64,
        capabilities: Capabilities,
    },
    /// Hub → runner, answer to `hello` (§3.2).
    Welcome {
        proto: u32,
        /// `true`: both streams continue. `false`: fresh streams, numbering restarts at 1.
        resumed: bool,
        resume_token: String,
        /// Highest up `seq` the hub has durably processed on this stream.
        last_up_seq: u64,
        heartbeat_ms: u64,
        max_inflight: u32,
    },
    /// Runner → hub: one TurnEvent envelope (§3.3). The envelope is typed by turn-event.md.
    Event {
        seq: u64,
        event_id: String,
        envelope: Value,
    },
    /// Runner → hub: a thread started on the runner (§3.4).
    ThreadOpen {
        seq: u64,
        event_id: String,
        thread_id: String,
        title: String,
        agent: String,
        workspace: Workspace,
    },
    /// Runner → hub: outcome of a command that could not be applied (§3.6).
    CommandResult {
        seq: u64,
        event_id: String,
        idempotency_key: String,
        ok: bool,
        code: CommandErrorCode,
        message: String,
    },
    /// Hub → runner: one command (§3.5).
    Command {
        seq: u64,
        idempotency_key: String,
        issued_at: String,
        command: Command,
    },
    /// Cumulative ack of the peer's stream (§3.7).
    Ack { seq: u64 },
    /// Hub → runner: the hub rejected one up frame; counts as an ack for that seq (§3.7).
    Nack {
        seq: u64,
        code: NackCode,
        message: String,
    },
    /// Runner → hub: best-effort presence hint (§3.7).
    Presence {
        state: PresenceState,
        reason: String,
    },
    /// Heartbeat request (§3.7). Serializes to `{"t":"ping"}`.
    Ping,
    /// Heartbeat answer (§3.7). Serializes to `{"t":"pong"}`.
    Pong,
    /// Reason for closing, sent just before the WebSocket close (§3.8).
    Close { code: CloseCode, message: String },
}

/// Strict parse of one tunnel frame (§6 step 1): unknown `t`, unknown fields and extra fields on
/// `ping` / `pong` are errors.
///
/// Use this on the wire. The derived `Deserialize` rejects unknown fields on every struct
/// variant, but serde does not apply `deny_unknown_fields` to the unit variants `ping` and
/// `pong` of an internally tagged enum, so `{"t":"ping","x":1}` would parse through serde alone.
pub fn parse_frame(input: &str) -> Result<Frame, serde_json::Error> {
    let value: Value = serde_json::from_str(input)?;
    if let Some(obj) = value.as_object() {
        let unit_tag = matches!(obj.get("t").and_then(Value::as_str), Some("ping" | "pong"));
        if unit_tag && obj.len() != 1 {
            return Err(<serde_json::Error as serde::de::Error>::custom(
                "`ping` and `pong` take no fields besides `t`",
            ));
        }
    }
    serde_json::from_value(value)
}

/// Runner capabilities reported in `hello` (§3.1).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Capabilities {
    /// `local` or `sandbox`.
    pub host_mode: String,
    pub harnesses: Vec<String>,
    pub max_sessions: u32,
    /// Supported spawn modes, e.g. `same_dir`, `worktree`.
    pub spawn: Vec<String>,
    pub features: Vec<String>,
}

/// Reason a hub rejected an up frame (§3.7). The turn-event.md §5 codes are `invalid_sequence`
/// and `invalid_envelope`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NackCode {
    InvalidEnvelope,
    InvalidSequence,
    /// The thread is not hosted by this machine.
    Forbidden,
    TooLarge,
}

/// Why a runner could not apply a command (§3.6).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CommandErrorCode {
    Capacity,
    UnknownThread,
    UnknownInterrupt,
    WorkspaceUntrusted,
    HarnessMissing,
    Unsupported,
    Invalid,
}

/// Runner-reported presence (§3.7).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PresenceState {
    /// The OS is about to sleep.
    Asleep,
    /// The runner is full.
    Busy,
}

/// Close reasons (§3.8). The WebSocket close code is `4000 + n`, where `n` is the position in
/// the table, starting at 0 for `normal`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CloseCode {
    Normal,
    Superseded,
    AuthInvalid,
    AuthRevoked,
    VersionUnsupported,
    ProtocolError,
    HeartbeatTimeout,
    ServerRestart,
    GoingToSleep,
}

impl CloseCode {
    /// Every close code, in table order.
    pub const ALL: [CloseCode; 9] = [
        CloseCode::Normal,
        CloseCode::Superseded,
        CloseCode::AuthInvalid,
        CloseCode::AuthRevoked,
        CloseCode::VersionUnsupported,
        CloseCode::ProtocolError,
        CloseCode::HeartbeatTimeout,
        CloseCode::ServerRestart,
        CloseCode::GoingToSleep,
    ];

    /// The WebSocket close code (§3.8).
    pub const fn ws_code(self) -> u16 {
        4000 + match self {
            Self::Normal => 0,
            Self::Superseded => 1,
            Self::AuthInvalid => 2,
            Self::AuthRevoked => 3,
            Self::VersionUnsupported => 4,
            Self::ProtocolError => 5,
            Self::HeartbeatTimeout => 6,
            Self::ServerRestart => 7,
            Self::GoingToSleep => 8,
        }
    }

    /// The close code for a WebSocket close code, if it is one of ours.
    pub fn from_ws_code(code: u16) -> Option<Self> {
        Self::ALL.into_iter().find(|c| c.ws_code() == code)
    }

    /// Runner reconnect policy from the §3.8 table: `Some(true)` reconnect with backoff,
    /// `Some(false)` do not reconnect, `None` runner → hub only (no reconnect decision).
    pub const fn reconnects(self) -> Option<bool> {
        match self {
            Self::Normal
            | Self::Superseded
            | Self::AuthInvalid
            | Self::AuthRevoked
            | Self::VersionUnsupported => Some(false),
            Self::ProtocolError | Self::HeartbeatTimeout | Self::ServerRestart => Some(true),
            Self::GoingToSleep => None,
        }
    }
}
