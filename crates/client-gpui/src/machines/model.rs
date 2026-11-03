//! Plain-Rust model of the Machines screen: rows from `GET /v1/machines`, the name rules of a
//! rename, and which row is being renamed or awaits a revoke confirmation. No GPUI in here.

use crate::agents::MachineInfo;

/// The longest machine name the hub accepts (`admin.rs` `MAX_NAME_CHARS`).
pub const MAX_NAME_CHARS: usize = 128;

/// A machine's presence (tunnel.md §4: online, asleep, offline).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PresenceState {
    Online,
    Asleep,
    Offline,
    Unknown,
}

impl PresenceState {
    pub fn parse(state: &str) -> Self {
        match state {
            "online" => Self::Online,
            "asleep" => Self::Asleep,
            "offline" => Self::Offline,
            _ => Self::Unknown,
        }
    }

    pub fn slug(self) -> &'static str {
        match self {
            Self::Online => "online",
            Self::Asleep => "asleep",
            Self::Offline => "offline",
            Self::Unknown => "unknown",
        }
    }
}

/// One row of the screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct MachineRow {
    pub machine_id: String,
    pub name: String,
    /// `local` or `sandbox`; empty when the hub did not say.
    pub kind: String,
    pub presence: PresenceState,
    /// When the presence started, as the hub sent it.
    pub since: Option<String>,
    pub harnesses: Vec<String>,
}

/// The rows for a `GET /v1/machines` answer, in the hub's order.
pub fn rows_from(machines: &[MachineInfo]) -> Vec<MachineRow> {
    machines
        .iter()
        .map(|machine| MachineRow {
            machine_id: machine.machine_id.clone(),
            name: machine.name.clone(),
            kind: machine.kind.clone(),
            presence: PresenceState::parse(&machine.presence.state),
            since: machine.presence.since.clone(),
            harnesses: machine
                .capabilities
                .as_ref()
                .map(|capabilities| capabilities.harnesses.clone())
                .unwrap_or_default(),
        })
        .collect()
}

/// Why a new name is refused before it is sent.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NameError {
    Empty,
    TooLong,
}

/// The name to send: trimmed, 1 to [`MAX_NAME_CHARS`] characters.
pub fn validate_name(raw: &str) -> Result<String, NameError> {
    let name = raw.trim();
    if name.is_empty() {
        return Err(NameError::Empty);
    }
    if name.chars().count() > MAX_NAME_CHARS {
        return Err(NameError::TooLong);
    }
    Ok(name.to_owned())
}

/// What the list shows besides the rows.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    /// Just the list.
    List,
    /// The row `machine_id` has its name field open.
    Renaming { machine_id: String },
    /// The row `machine_id` asks "revoke it?".
    ConfirmRevoke { machine_id: String },
}

/// A request in flight or just done.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Rename { machine_id: String, name: String },
    Revoke { machine_id: String, name: String },
}

/// A refused or failed request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    /// The HTTP status, absent when the hub could not be reached.
    pub status: Option<u16>,
    pub detail: String,
}
