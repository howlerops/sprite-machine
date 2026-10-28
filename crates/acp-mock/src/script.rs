//! The JSON script that drives the mock: turns, steps, and their validation.
//!
//! Every ACP payload in a script is checked against the `protocol::acp` types when the
//! script is loaded, so a malformed script fails before any client connects.

use std::path::Path;

use anyhow::{Context, Result, anyhow, bail};
use protocol::acp::{
    CreateElicitationRequest, RequestPermissionRequest, SessionConfigOption, SessionModeState,
    SessionUpdate, StopReason,
};
use serde_json::{Map, Value, json};

/// A fake Anthropic-style API key for redaction tests. It is not a credential.
pub const FAKE_API_KEY: &str =
    "sk-ant-api03-FAKEFAKEFAKEFAKEFAKEFAKEFAKEFAKE-FAKEFAKEFAKEFAKEFAKEFAKEFAKE";

/// Session id stored in a payload while it is validated at load time.
const PLACEHOLDER_SESSION: &str = "sess_placeholder";

/// A parsed script: `turns[n]` runs for the (n+1)th `session/prompt` of each session.
#[derive(Debug, Clone)]
pub struct Script {
    turns: Vec<Turn>,
    modes: Option<SessionModeState>,
    config_options: Option<Vec<SessionConfigOption>>,
}

/// One prompt turn: steps executed in order.
#[derive(Debug, Clone)]
pub struct Turn {
    steps: Vec<Step>,
}

/// One scripted action inside a turn.
#[derive(Debug, Clone)]
pub enum Step {
    /// A `session/update` notification (chunks, tool calls, plan, usage, commands, mode).
    Update(Box<SessionUpdate>),
    /// `session/request_permission`. `request` is the ACP payload without `sessionId`;
    /// `expect` is what the client must answer: `any`, `cancelled`, `allow`, `reject`,
    /// or an option id.
    Permission { request: Value, expect: String },
    /// `elicitation/create`. `request` is the ACP payload (`mode`, `message`, ...);
    /// `expect` is `any`, `accept`, `decline` or `cancel`.
    Elicitation { request: Value, expect: String },
    /// Client `terminal/create`, a `tool_call_update` carrying the terminal, then
    /// `terminal/wait_for_exit`, `terminal/output` and `terminal/release`.
    Terminal {
        command: String,
        tool_call_id: String,
    },
    /// Ends the turn with this stop reason. Must be the last step.
    Stop(StopReason),
}

impl Script {
    /// Parses and validates a script from JSON text.
    pub fn from_json(text: &str) -> Result<Self> {
        let value: Value = serde_json::from_str(text).context("script is not valid JSON")?;
        Self::from_value(&value)
    }

    /// Reads, parses and validates a script file.
    pub fn from_path(path: &Path) -> Result<Self> {
        let text = std::fs::read_to_string(path)
            .with_context(|| format!("reading script {}", path.display()))?;
        Self::from_json(&text).with_context(|| format!("in script {}", path.display()))
    }

    /// Parses and validates a script from an already-parsed JSON value.
    pub fn from_value(value: &Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("script must be a JSON object"))?;
        reject_unknown_keys(object, &["turns", "modes", "config_options"], "script")?;
        let turns = object
            .get("turns")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("script needs a \"turns\" array"))?;
        let turns = turns
            .iter()
            .enumerate()
            .map(|(index, turn)| Turn::from_value(turn).with_context(|| format!("turn {index}")))
            .collect::<Result<Vec<_>>>()?;
        let modes = object
            .get("modes")
            .map(|modes| {
                serde_json::from_value(modes.clone()).context("\"modes\" is not an ACP mode state")
            })
            .transpose()?;
        let config_options = object
            .get("config_options")
            .map(|options| {
                serde_json::from_value(options.clone())
                    .context("\"config_options\" is not an ACP config option list")
            })
            .transpose()?;
        Ok(Self {
            turns,
            modes,
            config_options,
        })
    }

    /// The turns, in prompt order.
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    /// The modes every session starts, resumes or loads with (`session/new`, `session/resume`,
    /// `session/load`). `None` when the script advertises none.
    pub fn modes(&self) -> Option<&SessionModeState> {
        self.modes.as_ref()
    }

    /// The config options every session starts, resumes or loads with. `None` when the script
    /// advertises none.
    pub fn config_options(&self) -> Option<&[SessionConfigOption]> {
        self.config_options.as_deref()
    }
}

impl Turn {
    fn from_value(value: &Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("a turn must be a JSON object"))?;
        reject_unknown_keys(object, &["steps"], "turn")?;
        let steps = object
            .get("steps")
            .and_then(Value::as_array)
            .ok_or_else(|| anyhow!("a turn needs a \"steps\" array"))?;
        let steps = steps
            .iter()
            .enumerate()
            .map(|(index, step)| Step::from_value(step).with_context(|| format!("step {index}")))
            .collect::<Result<Vec<_>>>()?;
        let stop_is_last = steps
            .iter()
            .enumerate()
            .all(|(index, step)| !matches!(step, Step::Stop(_)) || index + 1 == steps.len());
        if !stop_is_last {
            bail!("a \"stop\" step must be the last step of its turn");
        }
        Ok(Self { steps })
    }

    /// The steps, in execution order.
    pub fn steps(&self) -> &[Step] {
        &self.steps
    }
}

impl Step {
    fn from_value(value: &Value) -> Result<Self> {
        let object = value
            .as_object()
            .ok_or_else(|| anyhow!("a step must be a JSON object"))?;
        let keys: Vec<&str> = object
            .keys()
            .map(String::as_str)
            .filter(|key| *key != "expect")
            .collect();
        let [key] = keys.as_slice() else {
            bail!("a step needs exactly one step key, found {keys:?}");
        };
        let key = *key;
        let payload = &object[key];
        let expect = object.get("expect");
        if expect.is_some() && !matches!(key, "request_permission" | "elicitation") {
            bail!("\"expect\" only applies to request_permission and elicitation");
        }

        match key {
            "text" => text_chunk("agent_message_chunk", string_payload(payload, key)?),
            "thought" => text_chunk("agent_thought_chunk", string_payload(payload, key)?),
            "leak_secret" => {
                if payload != &Value::Bool(true) {
                    bail!("\"leak_secret\" must be true");
                }
                let text = format!("Debug output, the key is {FAKE_API_KEY}");
                text_chunk("agent_message_chunk", &text)
            }
            "tool_call" => Ok(Self::Update(Box::new(typed_update(
                "tool_call",
                object_value(payload, key)?,
            )?))),
            "tool_call_update" => Ok(Self::Update(Box::new(typed_update(
                "tool_call_update",
                object_value(payload, key)?,
            )?))),
            "plan" => {
                let entries = array_payload(payload, key)?;
                Ok(Self::Update(Box::new(typed_update(
                    "plan",
                    json!({ "entries": entries }),
                )?)))
            }
            "usage" => Ok(Self::Update(Box::new(typed_update(
                "usage_update",
                object_value(payload, key)?,
            )?))),
            "available_commands" => {
                let commands = array_payload(payload, key)?;
                Ok(Self::Update(Box::new(typed_update(
                    "available_commands_update",
                    json!({ "availableCommands": commands }),
                )?)))
            }
            "config_option" => {
                let options = array_payload(payload, key)?;
                Ok(Self::Update(Box::new(typed_update(
                    "config_option_update",
                    json!({ "configOptions": options }),
                )?)))
            }
            "current_mode" => {
                let mode = string_payload(payload, key)?;
                Ok(Self::Update(Box::new(typed_update(
                    "current_mode_update",
                    json!({ "currentModeId": mode }),
                )?)))
            }
            "request_permission" => {
                let typed: RequestPermissionRequest =
                    serde_json::from_value(with_placeholder_session(object_payload(payload, key)?))
                        .context("invalid request_permission payload")?;
                let mut allowed: Vec<String> = ["any", "cancelled", "allow", "reject"]
                    .map(String::from)
                    .to_vec();
                allowed.extend(
                    typed
                        .options
                        .iter()
                        .map(|option| option.option_id.0.to_string()),
                );
                let allowed: Vec<&str> = allowed.iter().map(String::as_str).collect();
                let expect = expect_value(expect, &allowed)?;
                Ok(Self::Permission {
                    request: payload.clone(),
                    expect,
                })
            }
            "elicitation" => {
                let _typed: CreateElicitationRequest =
                    serde_json::from_value(with_placeholder_session(object_payload(payload, key)?))
                        .context("invalid elicitation payload")?;
                let expect = expect_value(expect, &["any", "accept", "decline", "cancel"])?;
                Ok(Self::Elicitation {
                    request: payload.clone(),
                    expect,
                })
            }
            "terminal" => {
                let object = object_payload(payload, key)?;
                reject_unknown_keys(object, &["command", "toolCallId"], "terminal step")?;
                let command = object
                    .get("command")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow!("a terminal step needs a \"command\" string"))?;
                let tool_call_id = object
                    .get("toolCallId")
                    .and_then(Value::as_str)
                    .unwrap_or("call_terminal");
                Ok(Self::Terminal {
                    command: command.to_owned(),
                    tool_call_id: tool_call_id.to_owned(),
                })
            }
            "stop" => {
                let reason = serde_json::from_value(payload.clone()).context(
                    "\"stop\" must be a stop reason such as \"end_turn\" or \"cancelled\"",
                )?;
                Ok(Self::Stop(reason))
            }
            other => bail!("unknown step key {other:?}"),
        }
    }
}

/// Builds a typed `session/update` body from the update kind and its ACP fields.
pub(crate) fn typed_update(kind: &str, fields: Value) -> Result<SessionUpdate> {
    let Value::Object(mut map) = fields else {
        bail!("{kind} fields must be a JSON object");
    };
    map.insert("sessionUpdate".to_owned(), Value::String(kind.to_owned()));
    serde_json::from_value(Value::Object(map)).with_context(|| format!("invalid {kind} update"))
}

fn text_chunk(kind: &str, text: &str) -> Result<Step> {
    let fields = json!({ "content": { "type": "text", "text": text } });
    Ok(Step::Update(Box::new(typed_update(kind, fields)?)))
}

fn string_payload<'a>(payload: &'a Value, key: &str) -> Result<&'a str> {
    payload
        .as_str()
        .ok_or_else(|| anyhow!("\"{key}\" must be a string"))
}

fn array_payload<'a>(payload: &'a Value, key: &str) -> Result<&'a Vec<Value>> {
    payload
        .as_array()
        .ok_or_else(|| anyhow!("\"{key}\" must be an array"))
}

fn object_payload<'a>(payload: &'a Value, key: &str) -> Result<&'a Map<String, Value>> {
    payload
        .as_object()
        .ok_or_else(|| anyhow!("\"{key}\" must be an object"))
}

fn object_value(payload: &Value, key: &str) -> Result<Value> {
    object_payload(payload, key).map(|object| Value::Object(object.clone()))
}

/// Copies an ACP payload and sets a placeholder `sessionId`, so it can be validated
/// against the protocol type before the real session id is known.
fn with_placeholder_session(payload: &Map<String, Value>) -> Value {
    let mut map = payload.clone();
    map.insert(
        "sessionId".to_owned(),
        Value::String(PLACEHOLDER_SESSION.to_owned()),
    );
    Value::Object(map)
}

/// Reads the `expect` field (default `any`) and checks it against the allowed values.
fn expect_value(expect: Option<&Value>, allowed: &[&str]) -> Result<String> {
    let expect = match expect {
        None => "any".to_owned(),
        Some(value) => value
            .as_str()
            .ok_or_else(|| anyhow!("\"expect\" must be a string"))?
            .to_owned(),
    };
    if allowed.contains(&expect.as_str()) {
        Ok(expect)
    } else {
        bail!("\"expect\" {expect:?} is not one of {allowed:?}")
    }
}

fn reject_unknown_keys(object: &Map<String, Value>, allowed: &[&str], what: &str) -> Result<()> {
    match object.keys().find(|key| !allowed.contains(&key.as_str())) {
        Some(key) => bail!("unknown key {key:?} in {what}"),
        None => Ok(()),
    }
}
