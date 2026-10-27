// TRACER STUB: replaced by M1A-006a (ACP→TurnEvent mapping in protocol::mapping::acp)
//! Minimal, hard-coded ACP → TurnEvent mapping for the tracer bullet (turn-event.md §4.2, §4.3,
//! §4.5). It works on the raw ACP JSON and covers only what the tracer script sends: text and
//! thought chunks, `tool_call`, `tool_call_update` (status/fields, end), and
//! `session/request_permission`. Anything else becomes a `Raw{source: "acp"}` event so nothing
//! is lost. The real mapping (golden fixtures for every §4 row) is M1A-006a.

use std::collections::BTreeSet;

use protocol::turn_event::{
    AssistantRole, Event, Interrupt, InterruptKind, InterruptOption, Location,
    PermissionOptionKind, Raw, RawEventSource, RawPayload, RawSource, ReasoningMessageContent,
    ReasoningMessageEnd, ReasoningMessageStart, ReasoningRole, TextMessageContent, TextMessageEnd,
    TextMessageStart, ToolCallArgs, ToolCallEnd, ToolCallEndStatus, ToolCallOpenStatus,
    ToolCallStart, ToolKind, is_valid_id,
};
use serde_json::{Map, Value};

/// One mapped event plus the ACP payload it came from (the envelope's `raw`).
#[derive(Debug, Clone, PartialEq)]
pub struct Mapped {
    pub event: Event,
    pub raw: Option<RawPayload>,
}

impl Mapped {
    /// An event without a `raw` payload.
    pub fn plain(event: impl Into<Event>) -> Self {
        Self {
            event: event.into(),
            raw: None,
        }
    }

    fn with_raw(event: impl Into<Event>, payload: &Value) -> Self {
        Self {
            event: event.into(),
            raw: Some(RawPayload {
                source: RawSource::Acp,
                payload: payload.clone(),
            }),
        }
    }
}

/// The text or reasoning message currently open (§4.2: open on the first chunk, close on the
/// next update of a different kind or at the end of the run).
#[derive(Debug, Clone)]
struct OpenMessage {
    message_id: String,
    reasoning: bool,
}

/// Per-task mapping state.
#[derive(Debug)]
pub struct AcpMapper {
    id_seed: String,
    next_id: u64,
    open: Option<OpenMessage>,
    /// Tool calls started and not yet ended.
    open_tool_calls: BTreeSet<String>,
    /// Every tool call id seen (a tool call is started at most once).
    known_tool_calls: BTreeSet<String>,
}

impl AcpMapper {
    /// `id_seed` makes generated ids unique across runs (e.g. a start timestamp).
    pub fn new(id_seed: impl Into<String>) -> Self {
        Self {
            id_seed: id_seed.into(),
            next_id: 0,
            open: None,
            open_tool_calls: BTreeSet::new(),
            known_tool_calls: BTreeSet::new(),
        }
    }

    /// A fresh id with the given type prefix (`msg_`, `rsn_`, `int_`, …).
    pub fn fresh_id(&mut self, prefix: &str) -> String {
        self.next_id += 1;
        format!("{prefix}{}_{}", self.id_seed, self.next_id)
    }

    /// Maps one ACP `session/update` `update` object.
    pub fn session_update(&mut self, update: &Value) -> Vec<Mapped> {
        let kind = update
            .get("sessionUpdate")
            .and_then(Value::as_str)
            .unwrap_or_default();
        match kind {
            "agent_message_chunk" => self.chunk(update, false),
            "agent_thought_chunk" => self.chunk(update, true),
            "tool_call" | "tool_call_update" => {
                let mut out = self.close_messages();
                out.extend(self.tool_call(update));
                out
            }
            _ => {
                let mut out = self.close_messages();
                out.push(raw_event(update));
                out
            }
        }
    }

    /// Maps an ACP `session/request_permission` request (`params`) to an approval
    /// `Interrupt`. Closes the open message and starts the tool call if it is new.
    pub fn permission(
        &mut self,
        params: &Value,
        interrupt_id: &str,
        expires_at: &str,
    ) -> Vec<Mapped> {
        let mut out = self.close_messages();
        let tool_call = params.get("toolCall").cloned().unwrap_or(Value::Null);
        let tool_call_id = tool_call
            .get("toolCallId")
            .and_then(Value::as_str)
            .map(tool_call_id);
        if let Some(id) = &tool_call_id
            && !self.known_tool_calls.contains(id)
        {
            out.extend(self.tool_call(&tool_call));
        }
        let title = tool_call
            .get("title")
            .and_then(Value::as_str)
            .unwrap_or("Permission requested")
            .to_owned();
        let options = params
            .get("options")
            .and_then(Value::as_array)
            .map(|options| options.iter().filter_map(permission_option).collect());
        out.push(Mapped::with_raw(
            Interrupt {
                interrupt_id: interrupt_id.to_owned(),
                kind: InterruptKind::Approval,
                title,
                message: None,
                expires_at: expires_at.to_owned(),
                tool_call_id,
                options,
                response_schema: None,
                url: None,
            },
            params,
        ));
        out
    }

    /// Closes the open text or reasoning message, if any.
    pub fn close_messages(&mut self) -> Vec<Mapped> {
        match self.open.take() {
            Some(OpenMessage {
                message_id,
                reasoning: false,
            }) => vec![Mapped::plain(TextMessageEnd { message_id })],
            Some(OpenMessage {
                message_id,
                reasoning: true,
            }) => vec![Mapped::plain(ReasoningMessageEnd { message_id })],
            None => Vec::new(),
        }
    }

    /// Closes every open tool call with `status` (§5 rule 5: before the task's final
    /// terminal event).
    pub fn close_tool_calls(&mut self, status: ToolCallEndStatus) -> Vec<Mapped> {
        std::mem::take(&mut self.open_tool_calls)
            .into_iter()
            .map(|tool_call_id| {
                Mapped::plain(ToolCallEnd {
                    tool_call_id,
                    status,
                })
            })
            .collect()
    }

    fn chunk(&mut self, update: &Value, reasoning: bool) -> Vec<Mapped> {
        let content = update.get("content").cloned().unwrap_or(Value::Null);
        let text = match (
            content.get("type").and_then(Value::as_str),
            content.get("text").and_then(Value::as_str),
        ) {
            (Some("text"), Some(text)) => text.to_owned(),
            // Non-text blocks produce no *Content in v1 (§4.2): keep them as Raw.
            _ => return vec![raw_event(update)],
        };
        let mut out = Vec::new();
        let same_kind = self
            .open
            .as_ref()
            .is_some_and(|open| open.reasoning == reasoning);
        if !same_kind {
            out.extend(self.close_messages());
            let message_id = self.fresh_id(if reasoning { "rsn_" } else { "msg_" });
            out.push(if reasoning {
                Mapped::plain(ReasoningMessageStart {
                    message_id: message_id.clone(),
                    role: ReasoningRole::Reasoning,
                })
            } else {
                Mapped::plain(TextMessageStart {
                    message_id: message_id.clone(),
                    role: AssistantRole::Assistant,
                })
            });
            self.open = Some(OpenMessage {
                message_id,
                reasoning,
            });
        }
        if text.is_empty() {
            return out;
        }
        let Some(open) = &self.open else {
            return out;
        };
        let message_id = open.message_id.clone();
        out.push(if reasoning {
            Mapped::with_raw(
                ReasoningMessageContent {
                    message_id,
                    delta: text,
                },
                update,
            )
        } else {
            Mapped::with_raw(
                TextMessageContent {
                    message_id,
                    delta: text,
                },
                update,
            )
        });
        out
    }

    /// `tool_call` / `tool_call_update` (§4.3): upsert, args, end. Tool call content (results,
    /// diffs, terminals) is not mapped by the tracer.
    fn tool_call(&mut self, update: &Value) -> Vec<Mapped> {
        let Some(raw_id) = update.get("toolCallId").and_then(Value::as_str) else {
            return vec![raw_event(update)];
        };
        let id = tool_call_id(raw_id);
        let status = update.get("status").and_then(Value::as_str);
        let mut out = Vec::new();
        let start = ToolCallStart {
            tool_call_id: id.clone(),
            title: update
                .get("title")
                .and_then(Value::as_str)
                .map(str::to_owned),
            kind: update
                .get("kind")
                .and_then(|kind| serde_json::from_value::<ToolKind>(kind.clone()).ok()),
            status: match status {
                Some("pending") => Some(ToolCallOpenStatus::Pending),
                Some("in_progress") => Some(ToolCallOpenStatus::InProgress),
                _ => None,
            },
            locations: update
                .get("locations")
                .and_then(Value::as_array)
                .map(|locations| locations.iter().filter_map(location).collect()),
            parent_message_id: None,
        };
        let is_new = self.known_tool_calls.insert(id.clone());
        let changes_fields = start.title.is_some()
            || start.kind.is_some()
            || start.status.is_some()
            || start.locations.is_some();
        if is_new || changes_fields {
            self.open_tool_calls.insert(id.clone());
            out.push(Mapped::with_raw(start, update));
        }
        if let Some(raw_input) = update.get("rawInput") {
            out.push(Mapped::plain(ToolCallArgs {
                tool_call_id: id.clone(),
                raw_input: raw_input.clone(),
            }));
        }
        let end = match status {
            Some("completed") => Some(ToolCallEndStatus::Completed),
            Some("failed") => Some(ToolCallEndStatus::Failed),
            _ => None,
        };
        if let Some(status) = end
            && self.open_tool_calls.remove(&id)
        {
            out.push(Mapped::with_raw(
                ToolCallEnd {
                    tool_call_id: id,
                    status,
                },
                update,
            ));
        }
        out
    }
}

/// ACP `toolCallId` verbatim when it is a valid Id, otherwise `tc_` + a hash of it (§2).
pub fn tool_call_id(acp_id: &str) -> String {
    if is_valid_id(acp_id) {
        return acp_id.to_owned();
    }
    // FNV-1a, 64 bit: stable and dependency-free.
    let hash = acp_id.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("tc_{hash:016x}")
}

fn raw_event(update: &Value) -> Mapped {
    let event = match update {
        Value::Object(map) => map.clone(),
        other => {
            let mut map = Map::new();
            map.insert("value".to_owned(), other.clone());
            map
        }
    };
    Mapped::plain(Raw {
        source: RawEventSource::Acp,
        child_thread_id: None,
        event,
    })
}

fn location(value: &Value) -> Option<Location> {
    Some(Location {
        path: value.get("path")?.as_str()?.to_owned(),
        line: value
            .get("line")
            .and_then(Value::as_u64)
            .filter(|l| *l >= 1),
    })
}

fn permission_option(value: &Value) -> Option<InterruptOption> {
    let option_id = value.get("optionId")?.as_str()?;
    if !is_valid_id(option_id) {
        return None;
    }
    Some(InterruptOption {
        option_id: option_id.to_owned(),
        name: value.get("name")?.as_str()?.to_owned(),
        kind: serde_json::from_value::<PermissionOptionKind>(value.get("kind")?.clone()).ok()?,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn types(mapped: &[Mapped]) -> Vec<&'static str> {
        mapped.iter().map(|m| m.event.type_name()).collect()
    }

    #[test]
    fn chunks_open_and_close_messages() {
        let mut mapper = AcpMapper::new("t");
        let text = json!({"sessionUpdate": "agent_message_chunk",
                          "content": {"type": "text", "text": "hi"}});
        let first = mapper.session_update(&text);
        assert_eq!(types(&first), ["TextMessageStart", "TextMessageContent"]);
        assert_eq!(types(&mapper.session_update(&text)), ["TextMessageContent"]);
        let tool = json!({"sessionUpdate": "tool_call", "toolCallId": "call_1",
                          "title": "Edit", "kind": "edit", "status": "pending"});
        assert_eq!(
            types(&mapper.session_update(&tool)),
            ["TextMessageEnd", "ToolCallStart"]
        );
        let done = json!({"sessionUpdate": "tool_call_update", "toolCallId": "call_1",
                          "status": "completed"});
        assert_eq!(types(&mapper.session_update(&done)), ["ToolCallEnd"]);
        assert!(
            mapper
                .close_tool_calls(ToolCallEndStatus::Failed)
                .is_empty()
        );
    }

    #[test]
    fn permission_becomes_an_approval_interrupt() {
        let mut mapper = AcpMapper::new("t");
        let params = json!({
            "sessionId": "s",
            "toolCall": {"toolCallId": "call_2", "title": "Run tests", "kind": "execute"},
            "options": [{"optionId": "allow", "name": "Allow", "kind": "allow_once"},
                        {"optionId": "reject", "name": "Deny", "kind": "reject_once"}]
        });
        let out = mapper.permission(&params, "int_1", "2026-10-27T22:10:00.000Z");
        assert_eq!(types(&out), ["ToolCallStart", "Interrupt"]);
        let Event::Interrupt(interrupt) = &out[1].event else {
            panic!("not an interrupt");
        };
        assert_eq!(interrupt.tool_call_id.as_deref(), Some("call_2"));
        assert_eq!(interrupt.options.as_ref().map(Vec::len), Some(2));
    }

    #[test]
    fn invalid_tool_call_ids_are_hashed() {
        assert_eq!(tool_call_id("call_1"), "call_1");
        let hashed = tool_call_id("call 1/with spaces");
        assert!(hashed.starts_with("tc_") && is_valid_id(&hashed));
    }
}
