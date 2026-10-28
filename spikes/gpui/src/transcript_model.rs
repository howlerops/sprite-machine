//! Pure view model for the transcript pane: folds TurnEvent envelopes into display rows.
//!
//! No GPUI here. One [`Row`] per rendered item:
//! - a user message per `UserMessage`;
//! - an assistant message per `message_id` (deltas accumulated);
//! - a reasoning block per `message_id` (accumulated, shown collapsed);
//! - a tool call per `tool_call_id` (upserted by `ToolCallStart`, closed by `ToolCallEnd`);
//! - a plan per `(task, run, activity_id)` (snapshot, then JSON-Patch updates);
//! - a usage line per usage `StateDelta` (the thread's usage state after the patch).
//!
//! Everything else (run lifecycle, presence, checkpoints, diffs, …) renders no row in this spike.

use std::collections::HashMap;

use protocol::tunnel::ContentPart;
use protocol::turn_event::{
    ActivityMode, ActivityType, Envelope, Event, PatchOp, PatchOpKind, PlanContent, PlanEntry,
    PlanStatus, StateScope, ToolCallEndStatus, ToolCallOpenStatus, ToolKind,
};
use serde_json::Value;

/// Status of a tool call row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

impl ToolStatus {
    fn as_str(self) -> &'static str {
        match self {
            ToolStatus::Pending => "pending",
            ToolStatus::InProgress => "in progress",
            ToolStatus::Completed => "completed",
            ToolStatus::Failed => "failed",
        }
    }
}

/// One rendered transcript item.
#[derive(Debug, Clone, PartialEq)]
pub enum Row {
    User {
        message_id: String,
        text: String,
    },
    Assistant {
        message_id: String,
        text: String,
        done: bool,
    },
    Reasoning {
        message_id: String,
        text: String,
        done: bool,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        kind: Option<ToolKind>,
        status: ToolStatus,
    },
    Plan {
        key: String,
        entries: Vec<PlanEntry>,
    },
    Usage {
        context_used: u64,
        context_size: u64,
        cost: f64,
    },
}

impl Row {
    /// The one-line text shown for this row (also its accessibility label).
    pub fn label(&self) -> String {
        match self {
            Row::User { text, .. } => format!("You: {text}"),
            Row::Assistant { text, .. } => format!("Assistant: {text}"),
            Row::Reasoning { text, .. } => {
                format!("Reasoning (collapsed, {} chars)", text.chars().count())
            }
            Row::ToolCall { title, status, .. } => {
                format!("Tool: {title} [{}]", status.as_str())
            }
            Row::Plan { entries, .. } => {
                let done = entries
                    .iter()
                    .filter(|entry| entry.status == PlanStatus::Completed)
                    .count();
                let current = entries
                    .iter()
                    .find(|entry| entry.status == PlanStatus::InProgress)
                    .map(|entry| format!(" (now: {})", entry.content))
                    .unwrap_or_default();
                format!("Plan: {done}/{} done{current}", entries.len())
            }
            Row::Usage {
                context_used,
                context_size,
                cost,
            } => format!("Usage: {context_used}/{context_size} tokens, ${cost:.4}"),
        }
    }
}

/// Incremental fold of envelopes into rows. [`TranscriptModel::apply`] is the only mutator.
#[derive(Debug, Default)]
pub struct TranscriptModel {
    rows: Vec<Row>,
    /// `"<kind>:<id>"` → index into `rows`, for upserts.
    index: HashMap<String, usize>,
    /// Current JSON document of each plan, by row key.
    plans: HashMap<String, Value>,
    /// The thread's usage state.
    usage: Value,
}

impl TranscriptModel {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn rows(&self) -> &[Row] {
        &self.rows
    }

    pub fn into_rows(self) -> Vec<Row> {
        self.rows
    }

    /// A model that already holds `rows` (the in-memory fixture rows). It has no upsert index,
    /// so it is not fed envelopes; the fixture feed starts from [`TranscriptModel::new`].
    pub fn from_rows(rows: Vec<Row>) -> Self {
        Self {
            rows,
            ..Self::default()
        }
    }

    /// Applies one envelope.
    pub fn apply(&mut self, envelope: &Envelope) {
        match &envelope.event {
            Event::UserMessage(message) => {
                let text = message
                    .parts
                    .iter()
                    .filter_map(|part| match part {
                        ContentPart::Text { text } => Some(text.as_str()),
                        ContentPart::File { .. } => None,
                    })
                    .collect::<Vec<_>>()
                    .join(" ");
                self.push(
                    format!("user:{}", message.message_id),
                    Row::User {
                        message_id: message.message_id.clone(),
                        text,
                    },
                );
            }
            Event::TextMessageStart(start) => {
                self.assistant(&start.message_id);
            }
            Event::TextMessageContent(content) => {
                if let Row::Assistant { text, .. } = self.assistant(&content.message_id) {
                    text.push_str(&content.delta);
                }
            }
            Event::TextMessageEnd(end) => {
                if let Row::Assistant { done, .. } = self.assistant(&end.message_id) {
                    *done = true;
                }
            }
            Event::ReasoningMessageStart(start) => {
                self.reasoning(&start.message_id);
            }
            Event::ReasoningMessageContent(content) => {
                if let Row::Reasoning { text, .. } = self.reasoning(&content.message_id) {
                    text.push_str(&content.delta);
                }
            }
            Event::ReasoningMessageEnd(end) => {
                if let Row::Reasoning { done, .. } = self.reasoning(&end.message_id) {
                    *done = true;
                }
            }
            Event::ToolCallStart(start) => {
                if let Row::ToolCall {
                    title,
                    kind,
                    status,
                    ..
                } = self.tool_call(&start.tool_call_id)
                {
                    if let Some(new_title) = &start.title {
                        title.clone_from(new_title);
                    }
                    if start.kind.is_some() {
                        *kind = start.kind;
                    }
                    if let Some(new_status) = start.status {
                        *status = match new_status {
                            ToolCallOpenStatus::Pending => ToolStatus::Pending,
                            ToolCallOpenStatus::InProgress => ToolStatus::InProgress,
                        };
                    }
                }
            }
            Event::ToolCallEnd(end) => {
                if let Row::ToolCall { status, .. } = self.tool_call(&end.tool_call_id) {
                    *status = match end.status {
                        ToolCallEndStatus::Completed => ToolStatus::Completed,
                        ToolCallEndStatus::Failed => ToolStatus::Failed,
                    };
                }
            }
            Event::Activity(activity) if activity.activity_type == ActivityType::Plan => {
                let key = format!(
                    "{}/{}/{}",
                    envelope.task_id.as_deref().unwrap_or_default(),
                    envelope.run_id.as_deref().unwrap_or_default(),
                    activity.activity_id
                );
                let doc = self.plans.entry(key.clone()).or_insert(Value::Null);
                match activity.mode {
                    ActivityMode::Snapshot | ActivityMode::Append => {
                        if let Some(content) = &activity.content {
                            *doc = Value::Object(content.clone());
                        }
                    }
                    ActivityMode::Patch => {
                        apply_patch(doc, activity.patch.as_deref().unwrap_or_default());
                    }
                }
                let entries = serde_json::from_value::<PlanContent>(doc.clone())
                    .map(|plan| plan.entries)
                    .unwrap_or_default();
                let slot = self.upsert(format!("plan:{key}"), || Row::Plan {
                    key: key.clone(),
                    entries: Vec::new(),
                });
                if let Row::Plan { entries: row, .. } = slot {
                    *row = entries;
                }
            }
            Event::StateSnapshot(snapshot) if snapshot.scope == StateScope::Usage => {
                self.usage = Value::Object(snapshot.snapshot.clone());
            }
            Event::StateDelta(delta) if delta.scope == StateScope::Usage => {
                apply_patch(&mut self.usage, &delta.patch);
                let number = |pointer: &str| self.usage.pointer(pointer).and_then(Value::as_f64);
                let row = Row::Usage {
                    context_used: number("/context_used").unwrap_or_default() as u64,
                    context_size: number("/context_size").unwrap_or_default() as u64,
                    cost: number("/cost/amount").unwrap_or_default(),
                };
                self.rows.push(row);
            }
            _ => {}
        }
    }

    fn push(&mut self, key: String, row: Row) {
        self.index.insert(key, self.rows.len());
        self.rows.push(row);
    }

    fn upsert(&mut self, key: String, make: impl FnOnce() -> Row) -> &mut Row {
        let next = self.rows.len();
        let ix = *self.index.entry(key).or_insert(next);
        if ix == next {
            self.rows.push(make());
        }
        &mut self.rows[ix]
    }

    fn assistant(&mut self, message_id: &str) -> &mut Row {
        self.upsert(format!("assistant:{message_id}"), || Row::Assistant {
            message_id: message_id.to_owned(),
            text: String::new(),
            done: false,
        })
    }

    fn reasoning(&mut self, message_id: &str) -> &mut Row {
        self.upsert(format!("reasoning:{message_id}"), || Row::Reasoning {
            message_id: message_id.to_owned(),
            text: String::new(),
            done: false,
        })
    }

    fn tool_call(&mut self, tool_call_id: &str) -> &mut Row {
        self.upsert(format!("tool:{tool_call_id}"), || Row::ToolCall {
            tool_call_id: tool_call_id.to_owned(),
            title: tool_call_id.to_owned(),
            kind: None,
            status: ToolStatus::Pending,
        })
    }
}

/// Builds the rows for a whole log (the pure entry point the pane uses).
pub fn build_rows<'a>(events: impl IntoIterator<Item = &'a Envelope>) -> Vec<Row> {
    let mut model = TranscriptModel::new();
    for envelope in events {
        model.apply(envelope);
    }
    model.into_rows()
}

/// Minimal RFC 6902 subset for view state: `add`, `replace`, `remove`. Failing ops are skipped.
fn apply_patch(doc: &mut Value, ops: &[PatchOp]) {
    for op in ops {
        match op.op {
            PatchOpKind::Replace => {
                if let (Some(slot), Some(value)) = (doc.pointer_mut(&op.path), &op.value) {
                    *slot = value.clone();
                }
            }
            PatchOpKind::Add => {
                if let Some(value) = &op.value {
                    add(doc, &op.path, value.clone());
                }
            }
            PatchOpKind::Remove => {
                if let Some((parent, last)) = op.path.rsplit_once('/') {
                    match doc.pointer_mut(parent) {
                        Some(Value::Object(map)) => {
                            map.remove(last);
                        }
                        Some(Value::Array(items)) => {
                            if let Ok(ix) = last.parse::<usize>()
                                && ix < items.len()
                            {
                                items.remove(ix);
                            }
                        }
                        _ => {}
                    }
                }
            }
            PatchOpKind::Move | PatchOpKind::Copy | PatchOpKind::Test => {}
        }
    }
}

fn add(doc: &mut Value, path: &str, value: Value) {
    let Some((parent, last)) = path.rsplit_once('/') else {
        return;
    };
    match doc.pointer_mut(parent) {
        Some(Value::Object(map)) => {
            map.insert(last.to_owned(), value);
        }
        Some(Value::Array(items)) => {
            if last == "-" {
                items.push(value);
            } else if let Ok(ix) = last.parse::<usize>()
                && ix <= items.len()
            {
                items.insert(ix, value);
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use agui_fixture::{Scenario, complete_log};
    use protocol::turn_event::{
        Activity, Actor, ActorKind, PlanPriority, StateDelta, StateSnapshot, TextMessageContent,
        TextMessageEnd, TextMessageStart, ToolCallEnd, ToolCallStart, UserMessage,
    };
    use serde_json::json;

    fn env(event: impl Into<Event>) -> Envelope {
        Envelope::new(
            "th_test",
            Some("tk_1".to_owned()),
            Some("run_1".to_owned()),
            "2026-10-27T22:00:00.000Z",
            event,
        )
    }

    fn start(id: &str, title: Option<&str>, status: Option<ToolCallOpenStatus>) -> Envelope {
        env(ToolCallStart {
            tool_call_id: id.to_owned(),
            title: title.map(str::to_owned),
            kind: title.map(|_| ToolKind::Read),
            status,
            locations: None,
            parent_message_id: None,
        })
    }

    fn delta(id: &str, text: &str) -> Envelope {
        env(TextMessageContent {
            message_id: id.to_owned(),
            delta: text.to_owned(),
        })
    }

    #[test]
    fn assistant_text_accumulates_per_message_id() {
        let events = [
            env(TextMessageStart {
                message_id: "m1".to_owned(),
                role: Default::default(),
            }),
            delta("m1", "Hello"),
            env(TextMessageStart {
                message_id: "m2".to_owned(),
                role: Default::default(),
            }),
            delta("m2", "Other"),
            delta("m1", ", world"),
            env(TextMessageEnd {
                message_id: "m1".to_owned(),
            }),
        ];
        let rows = build_rows(&events);
        assert_eq!(rows.len(), 2);
        assert_eq!(
            rows[0],
            Row::Assistant {
                message_id: "m1".to_owned(),
                text: "Hello, world".to_owned(),
                done: true,
            }
        );
        assert_eq!(rows[1].label(), "Assistant: Other");
    }

    #[test]
    fn tool_calls_upsert_and_close_by_id() {
        let events = [
            start("tc1", Some("Read a.rs"), Some(ToolCallOpenStatus::Pending)),
            start("tc2", Some("Read b.rs"), Some(ToolCallOpenStatus::Pending)),
            start("tc1", None, Some(ToolCallOpenStatus::InProgress)),
            env(ToolCallEnd {
                tool_call_id: "tc1".to_owned(),
                status: ToolCallEndStatus::Completed,
            }),
            env(ToolCallEnd {
                tool_call_id: "tc2".to_owned(),
                status: ToolCallEndStatus::Failed,
            }),
        ];
        let mut model = TranscriptModel::new();
        model.apply(&events[0]);
        model.apply(&events[1]);
        model.apply(&events[2]);
        assert_eq!(model.rows().len(), 2);
        assert_eq!(model.rows()[0].label(), "Tool: Read a.rs [in progress]");
        model.apply(&events[3]);
        model.apply(&events[4]);
        assert_eq!(model.rows().len(), 2);
        assert_eq!(model.rows()[0].label(), "Tool: Read a.rs [completed]");
        assert_eq!(model.rows()[1].label(), "Tool: Read b.rs [failed]");
        assert!(matches!(
            &model.rows()[0],
            Row::ToolCall { kind: Some(ToolKind::Read), title, .. } if title == "Read a.rs"
        ));
    }

    #[test]
    fn plan_snapshot_then_patch_updates_one_row() {
        let plan = |mode, content: Option<Value>, patch: Option<Vec<PatchOp>>| {
            env(Activity {
                activity_id: "plan".to_owned(),
                activity_type: ActivityType::Plan,
                mode,
                content: content.and_then(|value| value.as_object().cloned()),
                patch,
            })
        };
        let snapshot = json!({"entries": [
            {"content": "Read", "priority": "high", "status": "in_progress"},
            {"content": "Edit", "priority": "low", "status": "pending"},
        ]});
        let patch = vec![PatchOp {
            op: PatchOpKind::Replace,
            path: "/entries/0/status".to_owned(),
            from: None,
            value: Some(json!("completed")),
        }];
        let rows = build_rows(&[
            plan(ActivityMode::Snapshot, Some(snapshot), None),
            plan(ActivityMode::Patch, None, Some(patch)),
        ]);
        assert_eq!(rows.len(), 1);
        let Row::Plan { entries, .. } = &rows[0] else {
            panic!("expected a plan row, got {:?}", rows[0]);
        };
        assert_eq!(entries[0].priority, PlanPriority::High);
        assert_eq!(rows[0].label(), "Plan: 1/2 done");
    }

    #[test]
    fn usage_rows_follow_the_patched_state() {
        let snapshot = json!({"context_used": 0, "context_size": 1000, "cost": {"amount": 0.0}});
        let usage_patch = vec![PatchOp {
            op: PatchOpKind::Replace,
            path: "/context_used".to_owned(),
            from: None,
            value: Some(json!(250)),
        }];
        let rows = build_rows(&[
            env(StateSnapshot {
                scope: StateScope::Usage,
                snapshot: snapshot.as_object().cloned().unwrap_or_default(),
            }),
            env(StateDelta {
                scope: StateScope::Usage,
                patch: usage_patch,
            }),
        ]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "Usage: 250/1000 tokens, $0.0000");
    }

    #[test]
    fn user_message_is_one_row() {
        let rows = build_rows(&[env(UserMessage {
            message_id: "u1".to_owned(),
            parts: vec![ContentPart::Text {
                text: "Hi".to_owned(),
            }],
            author: Actor {
                kind: ActorKind::User,
                id: "usr".to_owned(),
                name: None,
                device_id: None,
            },
        })]);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].label(), "You: Hi");
    }

    #[test]
    fn transcript_10k_folds_into_eight_rows_per_task() {
        let log = complete_log(Scenario::Transcript10k, "th_spike").expect("fixture log");
        assert!(log.len() >= 10_000, "fixture has {} events", log.len());
        let tasks = log
            .iter()
            .filter(|envelope| matches!(envelope.event, Event::UserMessage(_)))
            .count();
        let rows = build_rows(&log);
        // user, reasoning, plan, assistant, three tool calls, usage.
        assert_eq!(rows.len(), tasks * 8);
        assert!(rows.len() < log.len() / 2, "rows fold events together");
        assert_eq!(rows[0].label(), "You: Work on the router (task 1).");
        assert_eq!(rows[1].label(), "Reasoning (collapsed, 63 chars)");
        assert_eq!(rows[2].label(), "Plan: 1/3 done (now: Update the router)");
        assert_eq!(
            rows[3].label(),
            "Assistant: I will read src/router.rs and then update it. Done soon."
        );
        assert_eq!(rows[4].label(), "Tool: Read src/router.rs [completed]");
        assert_eq!(rows[6].label(), "Tool: cargo test -p demo [completed]");
        // Every fifth task's test run fails.
        assert_eq!(rows[5 * 8 - 2].label(), "Tool: cargo test -p demo [failed]");
        assert!(rows[7].label().starts_with("Usage: 1350/200000 tokens"));
    }
}
