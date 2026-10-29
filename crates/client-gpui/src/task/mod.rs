//! `task`: the task view. M3-008 adds the transcript pane (messages, reasoning, tool calls,
//! plan, terminal activity); M3-009 adds the plan view and the usage meters; later items add
//! the diff, approval and composer parts.

pub mod plan;
pub mod tools;
pub mod transcript;
pub mod usage;

pub use plan::{PlanModel, PlanRow, render_plan};
pub use transcript::{
    Row, RowKind, Tone, TranscriptEvent, TranscriptPane, thread_rows, thread_rows_expanded,
};
pub use usage::{Money, UsageModel, UsageSnapshot, UsageTracker, format_tokens, render_usage};
