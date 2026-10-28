//! `task`: the task view. M3-008 adds the transcript pane (messages, reasoning, tool calls,
//! plan, terminal activity); later items add the diff, approval and composer parts.

pub mod tools;
pub mod transcript;

pub use transcript::{Row, RowKind, Tone, TranscriptPane, thread_rows, thread_rows_expanded};
