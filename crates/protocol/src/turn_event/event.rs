//! Every TurnEvent variant (§4), one struct each, wrapped by [`Event`] (tagged by `type`).
//!
//! Optional fields are omitted when `None`. Fields whose JSON `null` is meaningful
//! (`raw_output`, `result`, patch `value`) keep a present `null` as `Some(Value::Null)`.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

use super::{Actor, ContentPart, present};

/// One variant from §4, discriminated by `type`.
///
/// Consumers must ignore unknown types (§2): they parse as [`Event::Unknown`], which carries
/// nothing and is not meant to be serialized back.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type")]
pub enum Event {
    UserMessage(UserMessage),
    RunStarted(RunStarted),
    RunFinished(RunFinished),
    RunError(RunError),
    TextMessageStart(TextMessageStart),
    TextMessageContent(TextMessageContent),
    TextMessageEnd(TextMessageEnd),
    ReasoningMessageStart(ReasoningMessageStart),
    ReasoningMessageContent(ReasoningMessageContent),
    ReasoningMessageEnd(ReasoningMessageEnd),
    ToolCallStart(ToolCallStart),
    ToolCallArgs(ToolCallArgs),
    ToolCallResult(ToolCallResult),
    ToolCallEnd(ToolCallEnd),
    Activity(Activity),
    Interrupt(Box<Interrupt>),
    InterruptResolved(InterruptResolved),
    StateSnapshot(StateSnapshot),
    StateDelta(StateDelta),
    SubagentStarted(Box<SubagentStarted>),
    SubagentFinished(SubagentFinished),
    SubagentError(SubagentError),
    StepStarted(StepStarted),
    StepFinished(StepFinished),
    Checkpoint(Checkpoint),
    Presence(Presence),
    Raw(Raw),
    /// A `type` this build does not know. Ignore it.
    #[serde(other)]
    Unknown,
}

impl Event {
    /// The `type` string of this variant (`"Unknown"` for [`Event::Unknown`]).
    pub fn type_name(&self) -> &'static str {
        match self {
            Self::UserMessage(_) => "UserMessage",
            Self::RunStarted(_) => "RunStarted",
            Self::RunFinished(_) => "RunFinished",
            Self::RunError(_) => "RunError",
            Self::TextMessageStart(_) => "TextMessageStart",
            Self::TextMessageContent(_) => "TextMessageContent",
            Self::TextMessageEnd(_) => "TextMessageEnd",
            Self::ReasoningMessageStart(_) => "ReasoningMessageStart",
            Self::ReasoningMessageContent(_) => "ReasoningMessageContent",
            Self::ReasoningMessageEnd(_) => "ReasoningMessageEnd",
            Self::ToolCallStart(_) => "ToolCallStart",
            Self::ToolCallArgs(_) => "ToolCallArgs",
            Self::ToolCallResult(_) => "ToolCallResult",
            Self::ToolCallEnd(_) => "ToolCallEnd",
            Self::Activity(_) => "Activity",
            Self::Interrupt(_) => "Interrupt",
            Self::InterruptResolved(_) => "InterruptResolved",
            Self::StateSnapshot(_) => "StateSnapshot",
            Self::StateDelta(_) => "StateDelta",
            Self::SubagentStarted(_) => "SubagentStarted",
            Self::SubagentFinished(_) => "SubagentFinished",
            Self::SubagentError(_) => "SubagentError",
            Self::StepStarted(_) => "StepStarted",
            Self::StepFinished(_) => "StepFinished",
            Self::Checkpoint(_) => "Checkpoint",
            Self::Presence(_) => "Presence",
            Self::Raw(_) => "Raw",
            Self::Unknown => "Unknown",
        }
    }

    /// Every known `type` string, in §4 order (27 variants, one schema each).
    pub const TYPE_NAMES: [&'static str; 27] = [
        "UserMessage",
        "RunStarted",
        "RunFinished",
        "RunError",
        "TextMessageStart",
        "TextMessageContent",
        "TextMessageEnd",
        "ReasoningMessageStart",
        "ReasoningMessageContent",
        "ReasoningMessageEnd",
        "ToolCallStart",
        "ToolCallArgs",
        "ToolCallResult",
        "ToolCallEnd",
        "Activity",
        "Interrupt",
        "InterruptResolved",
        "StateSnapshot",
        "StateDelta",
        "SubagentStarted",
        "SubagentFinished",
        "SubagentError",
        "StepStarted",
        "StepFinished",
        "Checkpoint",
        "Presence",
        "Raw",
    ];
}

macro_rules! into_event {
    ($($variant:ident),* $(,)?) => {
        $(impl From<$variant> for Event {
            fn from(value: $variant) -> Self {
                Event::$variant(value)
            }
        })*
    };
    (boxed: $($variant:ident),* $(,)?) => {
        $(impl From<$variant> for Event {
            fn from(value: $variant) -> Self {
                Event::$variant(Box::new(value))
            }
        })*
    };
}

into_event!(
    UserMessage,
    RunStarted,
    RunFinished,
    RunError,
    TextMessageStart,
    TextMessageContent,
    TextMessageEnd,
    ReasoningMessageStart,
    ReasoningMessageContent,
    ReasoningMessageEnd,
    ToolCallStart,
    ToolCallArgs,
    ToolCallResult,
    ToolCallEnd,
    Activity,
    InterruptResolved,
    StateSnapshot,
    StateDelta,
    SubagentFinished,
    SubagentError,
    StepStarted,
    StepFinished,
    Checkpoint,
    Presence,
    Raw,
);
into_event!(boxed: Interrupt, SubagentStarted);

// ---- 4.1 Lifecycle ------------------------------------------------------------------------

/// The prompt that opens a task (scope: task).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserMessage {
    pub message_id: String,
    /// At least one part.
    pub parts: Vec<ContentPart>,
    pub author: Actor,
}

/// A run begins (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunStarted {
    pub reason: RunReason,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_run_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub machine_id: Option<String>,
}

/// Why a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunReason {
    Prompt,
    Resume,
    Retry,
}

/// A run ended normally or paused on interrupts (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunFinished {
    pub outcome: RunOutcome,
}

/// How a run finished.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RunOutcome {
    Success {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        stop_reason: Option<StopReason>,
    },
    /// Every interrupt raised in this run that is still open (≥ 1).
    Interrupt { interrupt_ids: Vec<String> },
}

/// ACP stop reasons that end a run successfully.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StopReason {
    EndTurn,
    MaxTokens,
    MaxTurnRequests,
    Refusal,
}

/// A run ended with an error or was cancelled (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RunError {
    pub reason: RunErrorReason,
    /// `expired`, `harness_exited`, `protocol`, `timeout`, `budget`, …
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    pub message: String,
}

/// Why a run errored.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunErrorReason {
    Cancelled,
    Error,
}

// ---- 4.2 Text and reasoning ---------------------------------------------------------------

/// `role` of a text message: always `"assistant"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum AssistantRole {
    #[default]
    #[serde(rename = "assistant")]
    Assistant,
}

/// `role` of a reasoning message: always `"reasoning"`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ReasoningRole {
    #[default]
    #[serde(rename = "reasoning")]
    Reasoning,
}

/// Opens an assistant message (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessageStart {
    pub message_id: String,
    pub role: AssistantRole,
}

/// A non-empty text chunk of an open assistant message (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessageContent {
    pub message_id: String,
    pub delta: String,
}

/// Closes an assistant message (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextMessageEnd {
    pub message_id: String,
}

/// Opens a reasoning block (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningMessageStart {
    pub message_id: String,
    pub role: ReasoningRole,
}

/// A non-empty text chunk of an open reasoning block (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningMessageContent {
    pub message_id: String,
    pub delta: String,
}

/// Closes a reasoning block (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReasoningMessageEnd {
    pub message_id: String,
}

// ---- 4.3 Tool calls -----------------------------------------------------------------------

/// Creates a tool call or merges changed fields into it (upsert, scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallStart {
    pub tool_call_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind: Option<ToolKind>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<ToolCallOpenStatus>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub locations: Option<Vec<Location>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_message_id: Option<String>,
}

/// ACP `ToolKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolKind {
    Read,
    Edit,
    Delete,
    Move,
    Search,
    Execute,
    Think,
    Fetch,
    SwitchMode,
    Other,
}

/// Status of a tool call that is still open.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallOpenStatus {
    Pending,
    InProgress,
}

/// A file location a tool call touches.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Location {
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub line: Option<u64>,
}

/// The full tool input (ACP `rawInput`; replaces earlier args, scope: run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallArgs {
    pub tool_call_id: String,
    pub raw_input: Value,
}

/// Tool output (replaces the stored result, scope: run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolCallResult {
    pub tool_call_id: String,
    pub content: Vec<ContentPart>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub raw_output: Option<Value>,
}

/// The tool call reached a final status (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolCallEnd {
    pub tool_call_id: String,
    pub status: ToolCallEndStatus,
}

/// Final tool call status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallEndStatus {
    Completed,
    Failed,
}

// ---- 4.4 Activity -------------------------------------------------------------------------

/// A structured activity keyed by `activity_id` (scope: run).
///
/// `content` is typed by `activity_type`; use [`Activity::content_as`] with [`DiffContent`],
/// [`PlanContent`], [`TerminalContent`] or [`PolicyContent`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Activity {
    pub activity_id: String,
    pub activity_type: ActivityType,
    pub mode: ActivityMode,
    /// Required for `snapshot` and `append`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub content: Option<Map<String, Value>>,
    /// Required for `patch` (RFC 6902).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub patch: Option<Vec<PatchOp>>,
}

impl Activity {
    /// Parses `content` as the typed content for this activity type.
    pub fn content_as<T: serde::de::DeserializeOwned>(&self) -> Option<serde_json::Result<T>> {
        self.content
            .as_ref()
            .map(|content| serde_json::from_value(Value::Object(content.clone())))
    }
}

/// Kind of activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityType {
    Diff,
    Plan,
    Terminal,
    Policy,
}

/// How an activity event applies to the stored activity.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityMode {
    Snapshot,
    Append,
    Patch,
}

/// `content` of a `diff` activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DiffContent {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    pub path: String,
    /// `None` (JSON `null` or absent) for a new file.
    #[serde(default)]
    pub old_text: Option<String>,
    pub new_text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub chunk: Option<Chunk>,
}

/// Position of a chunk in a split diff.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Chunk {
    pub index: u64,
    pub last: bool,
}

/// `content` of a `plan` activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanContent {
    pub entries: Vec<PlanEntry>,
}

/// One plan entry.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PlanEntry {
    pub content: String,
    pub priority: PlanPriority,
    pub status: PlanStatus,
}

/// Plan entry priority.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanPriority {
    High,
    Medium,
    Low,
}

/// Plan entry status.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanStatus {
    Pending,
    InProgress,
    Completed,
}

/// `content` of a `terminal` activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalContent {
    pub terminal_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit: Option<TerminalExit>,
}

/// How a terminal command ended.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TerminalExit {
    #[serde(default)]
    pub exit_code: Option<i64>,
    #[serde(default)]
    pub signal: Option<String>,
}

/// `content` of a `policy` activity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PolicyContent {
    pub tool_call_id: String,
    pub decision: PolicyDecision,
    pub rule: String,
}

/// An auto-allow decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyDecision {
    Allow,
    Deny,
}

/// One RFC 6902 operation.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct PatchOp {
    pub op: PatchOpKind,
    pub path: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub from: Option<String>,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub value: Option<Value>,
}

/// RFC 6902 operation names.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PatchOpKind {
    Add,
    Remove,
    Replace,
    Move,
    Copy,
    Test,
}

// ---- 4.5 Interrupts -----------------------------------------------------------------------

/// The run needs a human (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Interrupt {
    pub interrupt_id: String,
    pub kind: InterruptKind,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    pub expires_at: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub options: Option<Vec<InterruptOption>>,
    /// form: JSON Schema of the answer.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub response_schema: Option<Map<String, Value>>,
    /// auth: URL to open.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

/// What kind of human input an interrupt needs.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum InterruptKind {
    Approval,
    Form,
    Auth,
}

/// An ACP `PermissionOption`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InterruptOption {
    pub option_id: String,
    pub name: String,
    pub kind: PermissionOptionKind,
}

/// ACP `PermissionOptionKind`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

/// The first accepted answer to an interrupt, or its cancellation / expiry (scope: task).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct InterruptResolved {
    pub interrupt_id: String,
    pub outcome: ResolvedOutcome,
    pub resolved_by: Actor,
}

/// How an interrupt was resolved.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResolvedOutcome {
    Selected { option_id: String },
    Submitted { data: Value },
    Cancelled,
    Expired,
    Declined,
}

// ---- 4.6 State ----------------------------------------------------------------------------

/// Full state for one scope (scope: thread).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateSnapshot {
    pub scope: StateScope,
    pub snapshot: Map<String, Value>,
}

/// An RFC 6902 patch against the last snapshot of one scope (scope: thread).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct StateDelta {
    pub scope: StateScope,
    pub patch: Vec<PatchOp>,
}

/// Which state a snapshot or delta is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StateScope {
    Session,
    Usage,
    Thread,
    Flow,
}

// ---- 4.7 Subagents and steps --------------------------------------------------------------

/// A child task on another agent started (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentStarted {
    pub subagent_run_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_thread_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_task_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agent_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_tool_call_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_subagent_run_id: Option<String>,
}

/// The child task completed (scope: run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct SubagentFinished {
    pub subagent_run_id: String,
    #[serde(
        default,
        deserialize_with = "present",
        skip_serializing_if = "Option::is_none"
    )]
    pub result: Option<Value>,
}

/// The child task failed or was cancelled (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SubagentError {
    pub subagent_run_id: String,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
}

/// A flow step began (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepStarted {
    /// Dotted path, e.g. `implement.verify`.
    pub step_id: String,
    pub step_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u64>,
}

/// A flow step ended (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StepFinished {
    pub step_id: String,
    pub step_name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub iteration: Option<u64>,
    pub outcome: StepOutcome,
}

/// How a flow step ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum StepOutcome {
    Success,
    Failed,
    Skipped,
    Cancelled,
}

/// A child thread's event nested in its parent, or an unmapped source event (scope: run).
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Raw {
    pub source: RawEventSource,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub child_thread_id: Option<String>,
    /// A full child envelope for `child`; the source event otherwise.
    pub event: Map<String, Value>,
}

/// Where a `Raw` event came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RawEventSource {
    Child,
    Acp,
    A2a,
    Flow,
}

// ---- 4.8 Checkpoints and presence ---------------------------------------------------------

/// A per-turn checkpoint was taken (scope: run).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Checkpoint {
    pub checkpoint_id: String,
    pub kind: CheckpointKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub git_ref: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup_handle: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub label: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub files_changed: Option<u64>,
}

/// Kind of checkpoint.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckpointKind {
    ShadowGit,
    Backup,
    Snapshot,
}

/// The machine hosting this thread changed presence (scope: thread).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Presence {
    pub machine_id: String,
    pub state: MachinePresence,
    pub since: String,
}

/// Machine presence as clients see it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MachinePresence {
    Online,
    Asleep,
    Offline,
}
