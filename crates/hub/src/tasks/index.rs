//! The thread's row in the D1 task index (M2-010), as the Thread DO sees it, and the rule for
//! when a request must refresh that row. Pure: the DO builds the row ([`crate::threads`]) and
//! writes it through [`super::sync`] (wasm32) or [`super::store::upsert`].

use protocol::turn_event::{Envelope, Event, StateScope};

/// The stored task statuses the Thread DO writes (`open`, `running`, `waiting`).
pub const STATUS_OPEN: &str = "open";
pub const STATUS_RUNNING: &str = "running";
pub const STATUS_WAITING: &str = "waiting";

/// The thread's index state at one point in time.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct IndexRow {
    pub thread_id: String,
    pub machine_id: String,
    pub agent: String,
    pub workspace_path: String,
    pub title: String,
    /// `user` (the client set the title) or `derived` (the first prompt's first line).
    pub title_source: &'static str,
    /// `open`, `running` or `waiting` (see the `STATUS_*` constants).
    pub status: &'static str,
    pub archived: bool,
    pub deleted: bool,
    /// The time of the change that produced this row (ISO-8601 UTC).
    pub updated_at: String,
    pub last_seq: i64,
}

/// `true` when a request may change the thread's title, status or lifecycle, so the index row
/// must be upserted after it. The request paths are the thread routes (client-api.md §5); the
/// events are the ones that move the status (a run starts or ends, an approval opens or is
/// answered), the ones that change the title, or a new prompt. Other appends (streamed output,
/// session state) leave the index alone, so a streamed reply does not write D1 per token.
pub fn changes_index(method: &str, path: &str, appended: &[Envelope]) -> bool {
    match method {
        "PATCH" | "DELETE" => return true,
        "POST" if path.ends_with("/init") || path.ends_with("/create") => return true,
        _ => {}
    }
    appended.iter().any(|envelope| match &envelope.event {
        Event::UserMessage(_)
        | Event::RunStarted(_)
        | Event::RunFinished(_)
        | Event::RunError(_)
        | Event::Interrupt(_)
        | Event::InterruptResolved(_) => true,
        Event::StateSnapshot(snapshot) => snapshot.scope == StateScope::Thread,
        Event::StateDelta(delta) => delta.scope == StateScope::Thread,
        _ => false,
    })
}
