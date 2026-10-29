//! The task transcript: the ordered items a task produced, built from its TurnEvents
//! (turn-event.md §4). Every item is an upsert keyed by its id, so a replayed or duplicated
//! event never adds a second item.

use std::collections::HashMap;

use protocol::tunnel::{Actor, ContentPart};
use protocol::turn_event::{
    Activity, ActivityMode, ActivityType, Checkpoint, DiffContent, Event, Interrupt, InterruptKind,
    InterruptOption, InterruptResolved, Location, PlanContent, PolicyContent, RawEventSource,
    StepOutcome, TerminalContent, ToolCallEndStatus, ToolKind,
};
use serde_json::{Map, Value};

use crate::patch;

use super::ViewError;

/// One thing shown in a task's transcript, in the order it first appeared.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    User(UserView),
    /// An assistant message, streamed by `message_id`.
    Text(TextView),
    /// A reasoning block, streamed by `message_id`.
    Reasoning(TextView),
    /// A tool call, upserted by `tool_call_id`.
    Tool(Box<ToolView>),
    /// A diff, plan, terminal or policy activity, keyed by `activity_id`.
    Activity(ActivityView),
    /// An interrupt (approval, form or auth) and its resolution, if any.
    Interrupt(Box<InterruptView>),
    Checkpoint(Checkpoint),
    /// A `RunError`: the run was cancelled or failed.
    Error {
        code: Option<String>,
        message: String,
    },
    /// A flow step, upserted by `step_id` and `iteration`.
    Step(StepView),
    /// A subagent run, upserted by `subagent_run_id`.
    Subagent(Box<SubagentView>),
    /// A `Raw` event. Not keyed: each event is one item.
    Raw(Box<RawView>),
}

/// A flow step: running until its `StepFinished`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepView {
    pub step_id: String,
    pub step_name: String,
    pub iteration: Option<u64>,
    /// `None` while the step runs.
    pub outcome: Option<StepOutcome>,
}

/// Where a subagent run stands.
#[derive(Debug, Clone, PartialEq)]
pub enum SubagentStatus {
    Running,
    /// `SubagentFinished`; `result` is what the child returned, if anything.
    Finished {
        result: Option<Value>,
    },
    /// `SubagentError`: the child failed or was cancelled.
    Failed {
        message: String,
        code: Option<String>,
    },
}

/// A subagent run, from `SubagentStarted` until `SubagentFinished` or `SubagentError`.
#[derive(Debug, Clone, PartialEq)]
pub struct SubagentView {
    pub subagent_run_id: String,
    /// Empty when only its `SubagentFinished` or `SubagentError` arrived.
    pub name: String,
    pub description: Option<String>,
    pub child_thread_id: Option<String>,
    pub child_task_id: Option<String>,
    pub agent_id: Option<String>,
    /// The parent tool call that delegated to this subagent, if any.
    pub parent_tool_call_id: Option<String>,
    /// The parent subagent run, when this one runs inside another.
    pub parent_subagent_run_id: Option<String>,
    pub status: SubagentStatus,
}

impl SubagentView {
    /// `true` until the subagent's `SubagentFinished` or `SubagentError`.
    pub fn is_open(&self) -> bool {
        matches!(self.status, SubagentStatus::Running)
    }
}

/// A `Raw` event: a child thread's event nested in its parent, or an unmapped source event.
#[derive(Debug, Clone, PartialEq)]
pub struct RawView {
    pub source: RawEventSource,
    pub child_thread_id: Option<String>,
    /// The nested child envelope for `Child`, else the source event, kept verbatim.
    pub event: Map<String, Value>,
}

/// The prompt that opened the task.
#[derive(Debug, Clone, PartialEq)]
pub struct UserView {
    pub message_id: String,
    /// Text parts joined; file parts are shown by name.
    pub text: String,
    pub author: Actor,
}

/// A streamed message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TextView {
    pub message_id: String,
    pub text: String,
    /// `false` after the matching `*End`.
    pub open: bool,
}

/// Where a tool call stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ToolStatus {
    Pending,
    InProgress,
    Completed,
    Failed,
}

/// A tool call, merged from its `ToolCallStart`, `Args`, `Result` and `End` events.
#[derive(Debug, Clone, PartialEq)]
pub struct ToolView {
    pub tool_call_id: String,
    pub title: Option<String>,
    pub kind: Option<ToolKind>,
    pub status: ToolStatus,
    pub locations: Vec<Location>,
    pub parent_message_id: Option<String>,
    pub raw_input: Option<Value>,
    pub content: Vec<ContentPart>,
    pub raw_output: Option<Value>,
}

impl ToolView {
    /// `true` until the tool call's `ToolCallEnd`.
    pub fn is_open(&self) -> bool {
        matches!(self.status, ToolStatus::Pending | ToolStatus::InProgress)
    }
}

/// An activity as the stored content object, with its typed accessors.
#[derive(Debug, Clone, PartialEq)]
pub struct ActivityView {
    pub activity_id: String,
    pub activity_type: ActivityType,
    /// The current content object (snapshot replaced, append concatenated, patch applied).
    pub content: Map<String, Value>,
    /// `false` while a chunked diff still has chunks to come.
    pub complete: bool,
}

impl ActivityView {
    pub fn diff(&self) -> Option<DiffContent> {
        self.typed()
    }

    pub fn plan(&self) -> Option<PlanContent> {
        self.typed()
    }

    pub fn terminal(&self) -> Option<TerminalContent> {
        self.typed()
    }

    pub fn policy(&self) -> Option<PolicyContent> {
        self.typed()
    }

    fn typed<T: serde::de::DeserializeOwned>(&self) -> Option<T> {
        serde_json::from_value(Value::Object(self.content.clone())).ok()
    }
}

/// A local answer that the outbound queue is still delivering (see `outbox`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LocalAnswer {
    /// Queued or being retried; the hub has not answered yet.
    Pending,
    /// The queue gave up on this answer (for example, the interrupt expired).
    Failed { message: String },
}

/// An interrupt and everything known about its answer.
#[derive(Debug, Clone, PartialEq)]
pub struct InterruptView {
    pub interrupt: Interrupt,
    /// The first accepted answer, from the hub's `InterruptResolved`.
    pub resolution: Option<InterruptResolved>,
    /// The `ts` of the envelope that carried `resolution` (RFC 3339, producer time, display
    /// only). `None` while open, and after a 409 alone, which carries no envelope of its own.
    pub resolved_at: Option<String>,
    /// Set when the hub answered `409 already_resolved`: who won (client-api.md §4).
    pub conflict: Option<Actor>,
    /// This device's own answer, while it is queued.
    pub local: Option<LocalAnswer>,
}

impl InterruptView {
    pub fn id(&self) -> &str {
        &self.interrupt.interrupt_id
    }

    pub fn kind(&self) -> InterruptKind {
        self.interrupt.kind
    }

    pub fn options(&self) -> &[InterruptOption] {
        self.interrupt.options.as_deref().unwrap_or(&[])
    }

    /// `true` while nobody has answered, as far as this client knows (a 409 counts as answered).
    pub fn is_open(&self) -> bool {
        self.resolution.is_none() && self.conflict.is_none()
    }

    /// Who answered, as far as this client knows: the hub's resolver, else the 409 winner.
    pub fn resolved_by(&self) -> Option<&Actor> {
        self.resolution
            .as_ref()
            .map(|resolution| &resolution.resolved_by)
            .or(self.conflict.as_ref())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
enum Key {
    User(String),
    Text(String),
    Reasoning(String),
    Tool(String),
    Activity(String),
    Interrupt(String),
    Checkpoint(String),
    Step(String, Option<u64>),
    Subagent(String),
}

/// The transcript of one task. Items keep first-appearance order.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Transcript {
    items: Vec<Item>,
    index: HashMap<Key, usize>,
}

impl Transcript {
    pub fn items(&self) -> &[Item] {
        &self.items
    }

    pub fn interrupt(&self, interrupt_id: &str) -> Option<&InterruptView> {
        match self.index.get(&Key::Interrupt(interrupt_id.to_owned())) {
            Some(&slot) => match &self.items[slot] {
                Item::Interrupt(view) => Some(view),
                _ => None,
            },
            None => None,
        }
    }

    pub fn interrupts(&self) -> impl Iterator<Item = &InterruptView> {
        self.items.iter().filter_map(|item| match item {
            Item::Interrupt(view) => Some(view.as_ref()),
            _ => None,
        })
    }

    /// Applies one TurnEvent, with the `ts` of the envelope that carried it. Events that are not
    /// transcript items are ignored.
    pub fn apply(&mut self, event: &Event, ts: &str) -> Result<(), ViewError> {
        match event {
            Event::UserMessage(message) => {
                let key = Key::User(message.message_id.clone());
                if !self.index.contains_key(&key) {
                    let view = UserView {
                        message_id: message.message_id.clone(),
                        text: join_parts(&message.parts),
                        author: message.author.clone(),
                    };
                    self.push(key, Item::User(view));
                }
            }
            Event::TextMessageStart(start) => {
                self.text_slot(Key::Text(start.message_id.clone()));
            }
            Event::TextMessageContent(content) => {
                let slot = self.text_slot(Key::Text(content.message_id.clone()));
                if let Item::Text(view) = &mut self.items[slot] {
                    view.text.push_str(&content.delta);
                }
            }
            Event::TextMessageEnd(end) => {
                self.close_text(Key::Text(end.message_id.clone()));
            }
            Event::ReasoningMessageStart(start) => {
                self.reasoning_slot(Key::Reasoning(start.message_id.clone()));
            }
            Event::ReasoningMessageContent(content) => {
                let slot = self.reasoning_slot(Key::Reasoning(content.message_id.clone()));
                if let Item::Reasoning(view) = &mut self.items[slot] {
                    view.text.push_str(&content.delta);
                }
            }
            Event::ReasoningMessageEnd(end) => {
                self.close_text(Key::Reasoning(end.message_id.clone()));
            }
            Event::ToolCallStart(start) => {
                let slot = self.tool_slot(&start.tool_call_id);
                if let Item::Tool(tool) = &mut self.items[slot] {
                    if let Some(title) = &start.title {
                        tool.title = Some(title.clone());
                    }
                    if let Some(kind) = start.kind {
                        tool.kind = Some(kind);
                    }
                    if let Some(locations) = &start.locations {
                        tool.locations = locations.clone();
                    }
                    if let Some(parent) = &start.parent_message_id {
                        tool.parent_message_id = Some(parent.clone());
                    }
                    // Once ToolCallEnd closed the call, nothing may reopen it (§5 rule 5).
                    if let (true, Some(status)) = (tool.is_open(), start.status) {
                        tool.status = match status {
                            protocol::turn_event::ToolCallOpenStatus::Pending => {
                                ToolStatus::Pending
                            }
                            protocol::turn_event::ToolCallOpenStatus::InProgress => {
                                ToolStatus::InProgress
                            }
                        };
                    }
                }
            }
            Event::ToolCallArgs(args) => {
                let slot = self.tool_slot(&args.tool_call_id);
                if let Item::Tool(tool) = &mut self.items[slot] {
                    tool.raw_input = Some(args.raw_input.clone());
                }
            }
            Event::ToolCallResult(result) => {
                let slot = self.tool_slot(&result.tool_call_id);
                if let Item::Tool(tool) = &mut self.items[slot] {
                    tool.content = result.content.clone();
                    tool.raw_output = result.raw_output.clone();
                }
            }
            Event::ToolCallEnd(end) => {
                let slot = self.tool_slot(&end.tool_call_id);
                if let Item::Tool(tool) = &mut self.items[slot] {
                    tool.status = match end.status {
                        ToolCallEndStatus::Completed => ToolStatus::Completed,
                        ToolCallEndStatus::Failed => ToolStatus::Failed,
                    };
                }
            }
            Event::Activity(activity) => self.apply_activity(activity)?,
            Event::Interrupt(interrupt) => {
                let key = Key::Interrupt(interrupt.interrupt_id.clone());
                if !self.index.contains_key(&key) {
                    let view = InterruptView {
                        interrupt: (**interrupt).clone(),
                        resolution: None,
                        resolved_at: None,
                        conflict: None,
                        local: None,
                    };
                    self.push(key, Item::Interrupt(Box::new(view)));
                }
            }
            Event::InterruptResolved(resolved) => {
                let key = Key::Interrupt(resolved.interrupt_id.clone());
                let found = self.index.get(&key).map(|&slot| &mut self.items[slot]);
                if let Some(Item::Interrupt(view)) = found {
                    // The first accepted answer is the one that counts, with its own envelope ts.
                    if view.resolution.is_none() {
                        view.resolution = Some(resolved.clone());
                        view.resolved_at = Some(ts.to_owned());
                    }
                    view.local = None;
                }
            }
            Event::Checkpoint(checkpoint) => {
                let key = Key::Checkpoint(checkpoint.checkpoint_id.clone());
                if !self.index.contains_key(&key) {
                    self.push(key, Item::Checkpoint(checkpoint.clone()));
                }
            }
            Event::RunError(error) => {
                self.items.push(Item::Error {
                    code: error.code.clone(),
                    message: error.message.clone(),
                });
            }
            Event::StepStarted(started) => {
                let slot = self.step_slot(&started.step_id, started.iteration);
                if let Item::Step(step) = &mut self.items[slot] {
                    step.step_name = started.step_name.clone();
                }
            }
            Event::StepFinished(finished) => {
                let slot = self.step_slot(&finished.step_id, finished.iteration);
                if let Item::Step(step) = &mut self.items[slot] {
                    step.step_name = finished.step_name.clone();
                    // The first outcome counts, as for an interrupt's resolution.
                    if step.outcome.is_none() {
                        step.outcome = Some(finished.outcome);
                    }
                }
            }
            Event::SubagentStarted(started) => {
                let slot = self.subagent_slot(&started.subagent_run_id);
                if let Item::Subagent(view) = &mut self.items[slot] {
                    view.name = started.name.clone();
                    if let Some(description) = &started.description {
                        view.description = Some(description.clone());
                    }
                    if let Some(child) = &started.child_thread_id {
                        view.child_thread_id = Some(child.clone());
                    }
                    if let Some(task) = &started.child_task_id {
                        view.child_task_id = Some(task.clone());
                    }
                    if let Some(agent) = &started.agent_id {
                        view.agent_id = Some(agent.clone());
                    }
                    if let Some(tool_call) = &started.parent_tool_call_id {
                        view.parent_tool_call_id = Some(tool_call.clone());
                    }
                    if let Some(parent) = &started.parent_subagent_run_id {
                        view.parent_subagent_run_id = Some(parent.clone());
                    }
                }
            }
            Event::SubagentFinished(finished) => {
                let slot = self.subagent_slot(&finished.subagent_run_id);
                if let Item::Subagent(view) = &mut self.items[slot]
                    && view.is_open()
                {
                    view.status = SubagentStatus::Finished {
                        result: finished.result.clone(),
                    };
                }
            }
            Event::SubagentError(error) => {
                let slot = self.subagent_slot(&error.subagent_run_id);
                if let Item::Subagent(view) = &mut self.items[slot]
                    && view.is_open()
                {
                    view.status = SubagentStatus::Failed {
                        message: error.message.clone(),
                        code: error.code.clone(),
                    };
                }
            }
            Event::Raw(raw) => {
                self.items.push(Item::Raw(Box::new(RawView {
                    source: raw.source,
                    child_thread_id: raw.child_thread_id.clone(),
                    event: raw.event.clone(),
                })));
            }
            _ => {}
        }
        Ok(())
    }

    /// Records the `409` winner for an interrupt this device answered. Returns `false` if the
    /// transcript has no such interrupt.
    pub fn note_conflict(&mut self, interrupt_id: &str, resolved_by: Actor) -> bool {
        match self.interrupt_mut(interrupt_id) {
            Some(view) => {
                view.conflict = Some(resolved_by);
                view.local = None;
                true
            }
            None => false,
        }
    }

    /// Sets (or clears, with `None`) this device's pending answer to an interrupt.
    pub fn set_local_answer(&mut self, interrupt_id: &str, local: Option<LocalAnswer>) -> bool {
        match self.interrupt_mut(interrupt_id) {
            Some(view) => {
                view.local = local;
                true
            }
            None => false,
        }
    }

    fn interrupt_mut(&mut self, interrupt_id: &str) -> Option<&mut InterruptView> {
        let slot = *self.index.get(&Key::Interrupt(interrupt_id.to_owned()))?;
        match &mut self.items[slot] {
            Item::Interrupt(view) => Some(view.as_mut()),
            _ => None,
        }
    }

    fn push(&mut self, key: Key, item: Item) -> usize {
        let slot = self.items.len();
        self.items.push(item);
        self.index.insert(key, slot);
        slot
    }

    fn text_slot(&mut self, key: Key) -> usize {
        if let Some(&slot) = self.index.get(&key) {
            return slot;
        }
        let id = match &key {
            Key::Text(id) => id.clone(),
            _ => String::new(),
        };
        self.push(
            key,
            Item::Text(TextView {
                message_id: id,
                text: String::new(),
                open: true,
            }),
        )
    }

    fn reasoning_slot(&mut self, key: Key) -> usize {
        if let Some(&slot) = self.index.get(&key) {
            return slot;
        }
        let id = match &key {
            Key::Reasoning(id) => id.clone(),
            _ => String::new(),
        };
        self.push(
            key,
            Item::Reasoning(TextView {
                message_id: id,
                text: String::new(),
                open: true,
            }),
        )
    }

    fn close_text(&mut self, key: Key) {
        if let Some(&slot) = self.index.get(&key) {
            match &mut self.items[slot] {
                Item::Text(view) | Item::Reasoning(view) => view.open = false,
                _ => {}
            }
        }
    }

    fn tool_slot(&mut self, tool_call_id: &str) -> usize {
        let key = Key::Tool(tool_call_id.to_owned());
        if let Some(&slot) = self.index.get(&key) {
            return slot;
        }
        self.push(
            key,
            Item::Tool(Box::new(ToolView {
                tool_call_id: tool_call_id.to_owned(),
                title: None,
                kind: None,
                status: ToolStatus::Pending,
                locations: Vec::new(),
                parent_message_id: None,
                raw_input: None,
                content: Vec::new(),
                raw_output: None,
            })),
        )
    }

    /// The slot of a step, created when it has none yet.
    fn step_slot(&mut self, step_id: &str, iteration: Option<u64>) -> usize {
        let key = Key::Step(step_id.to_owned(), iteration);
        if let Some(&slot) = self.index.get(&key) {
            return slot;
        }
        self.push(
            key,
            Item::Step(StepView {
                step_id: step_id.to_owned(),
                step_name: String::new(),
                iteration,
                outcome: None,
            }),
        )
    }

    /// The slot of a subagent run, created when it has none yet.
    fn subagent_slot(&mut self, subagent_run_id: &str) -> usize {
        let key = Key::Subagent(subagent_run_id.to_owned());
        if let Some(&slot) = self.index.get(&key) {
            return slot;
        }
        self.push(
            key,
            Item::Subagent(Box::new(SubagentView {
                subagent_run_id: subagent_run_id.to_owned(),
                name: String::new(),
                description: None,
                child_thread_id: None,
                child_task_id: None,
                agent_id: None,
                parent_tool_call_id: None,
                parent_subagent_run_id: None,
                status: SubagentStatus::Running,
            })),
        )
    }

    /// Whether [`Transcript::apply`] would refuse `event`. Changes nothing.
    pub(super) fn check(&self, event: &Event) -> Result<(), ViewError> {
        match event {
            Event::Activity(activity) => self.activity_content(activity).map(|_| ()),
            _ => Ok(()),
        }
    }

    fn apply_activity(&mut self, activity: &Activity) -> Result<(), ViewError> {
        let key = Key::Activity(activity.activity_id.clone());
        let slot = self.index.get(&key).copied();
        if let Some(slot) = slot
            && !matches!(self.items[slot], Item::Activity(_))
        {
            return Ok(());
        }
        // Everything that can fail happens in `activity_content`, before the item changes.
        let content = self.activity_content(activity)?;
        let complete = activity
            .content
            .as_ref()
            .and_then(|incoming| incoming.get("chunk"))
            .and_then(|chunk| chunk.get("last"))
            .and_then(Value::as_bool)
            .unwrap_or(true);
        match slot {
            Some(slot) => {
                if let Item::Activity(view) = &mut self.items[slot] {
                    view.activity_type = activity.activity_type;
                    view.complete = complete;
                    view.content = content;
                }
            }
            None => {
                self.push(
                    key,
                    Item::Activity(ActivityView {
                        activity_id: activity.activity_id.clone(),
                        activity_type: activity.activity_type,
                        content,
                        complete,
                    }),
                );
            }
        }
        Ok(())
    }

    /// The content an `Activity` leaves its item with. Changes nothing, so a patch that does not
    /// apply is refused before the transcript is touched.
    fn activity_content(&self, activity: &Activity) -> Result<Map<String, Value>, ViewError> {
        let incoming = activity.content.clone().unwrap_or_default();
        match activity.mode {
            ActivityMode::Snapshot => Ok(incoming),
            ActivityMode::Append => {
                let mut content = self.activity_content_so_far(&activity.activity_id);
                for (name, value) in incoming {
                    let concatenated = match (name.as_str(), content.get(&name), &value) {
                        ("output" | "new_text", Some(Value::String(old)), Value::String(new)) => {
                            Some(Value::String(format!("{old}{new}")))
                        }
                        _ => None,
                    };
                    content.insert(name, concatenated.unwrap_or(value));
                }
                Ok(content)
            }
            ActivityMode::Patch => {
                let mut document =
                    Value::Object(self.activity_content_so_far(&activity.activity_id));
                let ops = activity.patch.as_deref().unwrap_or(&[]);
                patch::apply(&mut document, ops)?;
                Ok(patch::as_object(document))
            }
        }
    }

    /// The content an activity holds now (empty when it has no item yet).
    fn activity_content_so_far(&self, activity_id: &str) -> Map<String, Value> {
        match self.index.get(&Key::Activity(activity_id.to_owned())) {
            Some(&slot) => match &self.items[slot] {
                Item::Activity(view) => view.content.clone(),
                _ => Map::new(),
            },
            None => Map::new(),
        }
    }
}

/// Text parts joined with newlines, file parts shown by name.
fn join_parts(parts: &[ContentPart]) -> String {
    let mut out = String::new();
    for part in parts {
        if !out.is_empty() {
            out.push('\n');
        }
        match part {
            ContentPart::Text { text } => out.push_str(text),
            ContentPart::File { name, .. } => {
                out.push_str("[file: ");
                out.push_str(name);
                out.push(']');
            }
        }
    }
    out
}
