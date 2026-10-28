//! Pure conversions from ACP values to TurnEvent values (no state).

use serde_json::{Map, Value, json};

use crate::acp;
use crate::turn_event::{self as te, ContentPart, is_valid_id};

/// ACP `toolCallId` verbatim when it is a valid Id, otherwise `tc_` + a hash of it (§2).
pub fn tool_call_id(acp_id: &str) -> String {
    mapped_id("tc_", acp_id)
}

/// `raw` verbatim when it is a valid Id (§2), otherwise `prefix` + a stable hash of it.
pub fn mapped_id(prefix: &str, raw: &str) -> String {
    if is_valid_id(raw) {
        return raw.to_owned();
    }
    // FNV-1a, 64 bit: stable and dependency-free.
    let hash = raw.bytes().fold(0xcbf2_9ce4_8422_2325_u64, |h, b| {
        (h ^ u64::from(b)).wrapping_mul(0x0000_0100_0000_01b3)
    });
    format!("{prefix}{hash:016x}")
}

/// Length of `c` once JSON-escaped inside a string literal.
fn escaped_char_len(c: char) -> usize {
    match c {
        '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
        c if (c as u32) < 0x20 => 6,
        c => c.len_utf8(),
    }
}

/// Length of `text` once JSON-escaped (without the quotes).
#[cfg(test)]
pub fn escaped_len(text: &str) -> usize {
    text.chars().map(escaped_char_len).sum()
}

/// Splits `text` into pieces whose JSON-escaped length is at most `budget`, never inside a
/// character. Always returns at least one piece (an empty text is one empty piece).
pub fn split_escaped(text: &str, budget: usize) -> Vec<&str> {
    let mut pieces = Vec::new();
    let mut start = 0;
    let mut used = 0;
    for (index, c) in text.char_indices() {
        let len = escaped_char_len(c);
        if used + len > budget && index > start {
            pieces.push(&text[start..index]);
            start = index;
            used = 0;
        }
        used += len;
    }
    pieces.push(&text[start..]);
    pieces
}

/// The longest suffix of `text` whose JSON-escaped length is at most `budget`.
pub fn tail_escaped(text: &str, budget: usize) -> &str {
    let mut used = 0;
    let mut start = text.len();
    for (index, c) in text.char_indices().rev() {
        used += escaped_char_len(c);
        if used > budget {
            break;
        }
        start = index;
    }
    &text[start..]
}

/// One ACP content block as a TurnEvent [`ContentPart`], or `None` when v1 has no part for it
/// (images, audio, binary resources, non-https links). The raw payload keeps those.
pub fn content_part(block: &acp::ContentBlock) -> Option<ContentPart> {
    match block {
        acp::ContentBlock::Text(text) => Some(ContentPart::Text {
            text: text.text.clone(),
        }),
        acp::ContentBlock::ResourceLink(link) if link.uri.starts_with("https://") => {
            Some(ContentPart::File {
                name: link.name.clone(),
                mime_type: link
                    .mime_type
                    .clone()
                    .unwrap_or_else(|| "application/octet-stream".to_owned()),
                url: link.uri.clone(),
                size: link.size.and_then(|size| u64::try_from(size).ok()),
                sha256: None,
            })
        }
        acp::ContentBlock::Resource(resource) => match &resource.resource {
            acp::EmbeddedResourceResource::TextResourceContents(text) => Some(ContentPart::Text {
                text: text.text.clone(),
            }),
            _ => None,
        },
        _ => None,
    }
}

pub fn tool_kind(kind: acp::ToolKind) -> te::ToolKind {
    match kind {
        acp::ToolKind::Read => te::ToolKind::Read,
        acp::ToolKind::Edit => te::ToolKind::Edit,
        acp::ToolKind::Delete => te::ToolKind::Delete,
        acp::ToolKind::Move => te::ToolKind::Move,
        acp::ToolKind::Search => te::ToolKind::Search,
        acp::ToolKind::Execute => te::ToolKind::Execute,
        acp::ToolKind::Think => te::ToolKind::Think,
        acp::ToolKind::Fetch => te::ToolKind::Fetch,
        acp::ToolKind::SwitchMode => te::ToolKind::SwitchMode,
        _ => te::ToolKind::Other,
    }
}

/// `Some` for the two statuses a tool call can still be open in.
pub fn open_status(status: acp::ToolCallStatus) -> Option<te::ToolCallOpenStatus> {
    match status {
        acp::ToolCallStatus::Pending => Some(te::ToolCallOpenStatus::Pending),
        acp::ToolCallStatus::InProgress => Some(te::ToolCallOpenStatus::InProgress),
        _ => None,
    }
}

/// `Some` for the two statuses that close a tool call.
pub fn end_status(status: acp::ToolCallStatus) -> Option<te::ToolCallEndStatus> {
    match status {
        acp::ToolCallStatus::Completed => Some(te::ToolCallEndStatus::Completed),
        acp::ToolCallStatus::Failed => Some(te::ToolCallEndStatus::Failed),
        _ => None,
    }
}

pub fn location(location: &acp::ToolCallLocation) -> te::Location {
    te::Location {
        path: location.path.to_string_lossy().into_owned(),
        line: location.line.filter(|line| *line >= 1).map(u64::from),
    }
}

pub fn permission_kind(kind: acp::PermissionOptionKind) -> Option<te::PermissionOptionKind> {
    match kind {
        acp::PermissionOptionKind::AllowOnce => Some(te::PermissionOptionKind::AllowOnce),
        acp::PermissionOptionKind::AllowAlways => Some(te::PermissionOptionKind::AllowAlways),
        acp::PermissionOptionKind::RejectOnce => Some(te::PermissionOptionKind::RejectOnce),
        acp::PermissionOptionKind::RejectAlways => Some(te::PermissionOptionKind::RejectAlways),
        _ => None,
    }
}

pub fn plan_content(plan: &acp::Plan) -> Map<String, Value> {
    let entries: Vec<Value> = plan
        .entries
        .iter()
        .map(|entry| {
            let priority = match entry.priority {
                acp::PlanEntryPriority::High => "high",
                acp::PlanEntryPriority::Low => "low",
                _ => "medium",
            };
            let status = match entry.status {
                acp::PlanEntryStatus::InProgress => "in_progress",
                acp::PlanEntryStatus::Completed => "completed",
                _ => "pending",
            };
            json!({"content": entry.content, "priority": priority, "status": status})
        })
        .collect();
    let mut content = Map::new();
    content.insert("entries".to_owned(), Value::Array(entries));
    content
}

/// `{name, description, input_hint?}` for each command (§4.6 `session` snapshot).
pub fn available_commands(update: &acp::AvailableCommandsUpdate) -> Value {
    Value::Array(
        update
            .available_commands
            .iter()
            .map(|command| {
                let mut entry = Map::new();
                entry.insert("name".to_owned(), json!(command.name));
                entry.insert("description".to_owned(), json!(command.description));
                if let Some(acp::AvailableCommandInput::Unstructured(input)) = &command.input {
                    entry.insert("input_hint".to_owned(), json!(input.hint));
                }
                Value::Object(entry)
            })
            .collect(),
    )
}

/// `{id, name, description?}` for each mode.
pub fn modes(modes: &[acp::SessionMode]) -> Value {
    Value::Array(
        modes
            .iter()
            .map(|mode| {
                let mut entry = Map::new();
                entry.insert("id".to_owned(), json!(&*mode.id.0));
                entry.insert("name".to_owned(), json!(mode.name));
                if let Some(description) = &mode.description {
                    entry.insert("description".to_owned(), json!(description));
                }
                Value::Object(entry)
            })
            .collect(),
    )
}

/// The ACP config options, as ACP serializes them.
pub fn config_options(options: &[acp::SessionConfigOption]) -> Value {
    Value::Array(
        options
            .iter()
            .filter_map(|option| serde_json::to_value(option).ok())
            .collect(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn invalid_ids_are_hashed_and_valid_ones_kept() {
        assert_eq!(tool_call_id("call_1"), "call_1");
        let hashed = tool_call_id("call 1/with spaces");
        assert!(hashed.starts_with("tc_") && is_valid_id(&hashed));
        assert_eq!(hashed, tool_call_id("call 1/with spaces"));
        assert!(is_valid_id(&tool_call_id(&"x".repeat(200))));
    }

    #[test]
    fn escaped_splitting_respects_budget_and_characters() {
        let text = "a\"b\nc\u{1}d é";
        assert_eq!(escaped_len(text), 1 + 2 + 1 + 2 + 1 + 6 + 1 + 1 + 2);
        let pieces = split_escaped(text, 6);
        assert_eq!(pieces.concat(), text);
        assert!(pieces.iter().all(|piece| escaped_len(piece) <= 6));
        assert_eq!(split_escaped("", 6), vec![""]);
        let tail = tail_escaped(text, 5);
        assert!(text.ends_with(tail) && escaped_len(tail) <= 5);
    }
}
