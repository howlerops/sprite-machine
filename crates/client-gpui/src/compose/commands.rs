//! Thread-composer decisions that need no window: the session options (slash commands, modes,
//! models) read from the thread's `session` state (turn-event.md §4.6), the `/` command list and
//! what Enter does with a command, which modes the runner allows, and the hub requests the
//! composer asks for (client-api.md §5.3, §5.5, §5.5a).

use client_core::outbox::Intent;
use serde_json::Value;

use super::new::mode_looseness;

/// One entry of `session.available_commands`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SlashCommand {
    pub name: String,
    pub description: String,
    /// ACP `input.hint`; a command with a hint takes arguments.
    pub input_hint: Option<String>,
}

/// One entry of `session.modes`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModeOption {
    pub id: String,
    pub name: String,
}

/// One value of a select-type config option.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChoiceOption {
    pub value: String,
    pub name: String,
}

/// A select-type entry of `session.config_options` (the model picker uses the one named
/// `model`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConfigSelect {
    pub id: String,
    pub name: String,
    pub current: Option<String>,
    pub options: Vec<ChoiceOption>,
}

/// What the thread's `session` state offers. Empty when the state is missing.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SessionOptions {
    pub commands: Vec<SlashCommand>,
    pub modes: Vec<ModeOption>,
    pub current_mode: Option<String>,
    pub model: Option<ConfigSelect>,
}

fn text(value: &Value, key: &str) -> Option<String> {
    value.get(key)?.as_str().map(str::to_owned)
}

fn parse_choices(options: &Value, out: &mut Vec<ChoiceOption>) {
    for entry in options.as_array().into_iter().flatten() {
        if let Some(group) = entry.get("options") {
            // A group of values (ACP `SessionConfigSelectGroup`).
            parse_choices(group, out);
        } else if let Some(value) = text(entry, "value") {
            let name = text(entry, "name").unwrap_or_else(|| value.clone());
            out.push(ChoiceOption { value, name });
        }
    }
}

fn parse_select(option: &Value) -> Option<ConfigSelect> {
    let id = text(option, "id")?;
    let mut options = Vec::new();
    parse_choices(option.get("options")?, &mut options);
    Some(ConfigSelect {
        name: text(option, "name").unwrap_or_else(|| id.clone()),
        current: text(option, "currentValue").or_else(|| text(option, "current_value")),
        id,
        options,
    })
}

impl SessionOptions {
    /// Read the `session` state document.
    pub fn from_state(state: Option<&Value>) -> Self {
        let Some(state) = state else {
            return Self::default();
        };
        let commands = state
            .get("available_commands")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let name = text(entry, "name")?;
                let hint = entry
                    .get("input_hint")
                    .and_then(Value::as_str)
                    .or_else(|| entry.pointer("/input/hint").and_then(Value::as_str))
                    .map(str::to_owned);
                Some(SlashCommand {
                    description: text(entry, "description").unwrap_or_default(),
                    input_hint: hint,
                    name,
                })
            })
            .collect();
        let modes = state
            .get("modes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .filter_map(|entry| {
                let id = text(entry, "id")?;
                Some(ModeOption {
                    name: text(entry, "name").unwrap_or_else(|| id.clone()),
                    id,
                })
            })
            .collect();
        let model = state
            .get("config_options")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .find(|option| {
                option.get("id").and_then(Value::as_str) == Some("model")
                    || option.get("category").and_then(Value::as_str) == Some("model")
            })
            .and_then(parse_select);
        Self {
            commands,
            modes,
            current_mode: text(state, "current_mode_id"),
            model,
        }
    }

    /// The modes the composer offers: the session's own, never looser than the runner's local
    /// mode `cap` (tunnel.md `capabilities.features` `permission_mode:<mode>`). Without a cap
    /// every session mode is offered; with one, a mode whose looseness is unknown is left out.
    pub fn allowed_modes(&self, cap: Option<&str>) -> Vec<&ModeOption> {
        let cap = cap.and_then(mode_looseness);
        self.modes
            .iter()
            .filter(|mode| match cap {
                None => true,
                Some(cap) => mode_looseness(&mode.id).is_some_and(|looseness| looseness <= cap),
            })
            .collect()
    }
}

/// The runner's local mode from a machine's `capabilities.features` (`permission_mode:<mode>`).
/// If several are listed, the strictest wins.
pub fn cap_from_features<S: AsRef<str>>(features: &[S]) -> Option<String> {
    features
        .iter()
        .filter_map(|feature| feature.as_ref().strip_prefix("permission_mode:"))
        .filter_map(|mode| mode_looseness(mode).map(|looseness| (looseness, mode)))
        .min()
        .map(|(_, mode)| mode.to_owned())
}

/// The command query in `text`: `Some("")` for a lone `/`, `Some("re")` for `/re`. `None` once
/// the user has typed a space or a newline (the command name is done) or the text is not a
/// command.
pub fn slash_query(text: &str) -> Option<&str> {
    let rest = text.strip_prefix('/')?;
    (!rest.contains(char::is_whitespace)).then_some(rest)
}

/// Commands matching `query`, case-insensitively: names starting with it first, then names that
/// contain it, then descriptions that do. Each group keeps the session's order.
pub fn filter_commands<'a>(commands: &'a [SlashCommand], query: &str) -> Vec<&'a SlashCommand> {
    let query = query.to_lowercase();
    let rank = |command: &SlashCommand| -> Option<u8> {
        let name = command.name.to_lowercase();
        if name.starts_with(&query) {
            Some(0)
        } else if name.contains(&query) {
            Some(1)
        } else if command.description.to_lowercase().contains(&query) {
            Some(2)
        } else {
            None
        }
    };
    let mut ranked: Vec<(u8, &SlashCommand)> = commands
        .iter()
        .filter_map(|command| rank(command).map(|rank| (rank, command)))
        .collect();
    ranked.sort_by_key(|(rank, _)| *rank);
    ranked.into_iter().map(|(_, command)| command).collect()
}

/// What Enter does with a chosen command (ACP: a command is a prompt whose text starts with
/// `/name`; one that takes input waits for its arguments).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SlashAccept {
    /// Put this text in the composer; the user types the arguments.
    Insert(String),
    /// Send this text as the prompt.
    Send(String),
}

pub fn accept_command(command: &SlashCommand) -> SlashAccept {
    if command.input_hint.is_some() {
        SlashAccept::Insert(format!("/{} ", command.name))
    } else {
        SlashAccept::Send(format!("/{}", command.name))
    }
}

/// What the thread composer asks its owner (the shell) to do.
#[derive(Debug, Clone, PartialEq)]
pub enum ComposerEvent {
    /// Send `text` as a prompt (queued by the hub when a task is running).
    Send { thread_id: String, text: String },
    /// Stop the running task: `Intent::Cancel`.
    Cancel(Intent),
    /// A session change (`Intent::SetSession`) or a queued-message removal
    /// (`Intent::RemovePending`): enqueue it in the outbox.
    Intent(Intent),
}
