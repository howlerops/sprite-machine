//! The inbox's model (plan §3, surface 2): the thread list split into sections, the debounced
//! search, the filters, and optimistic archive / rename / quick-answer with rollback. No GPUI and
//! no network in here; the view and `api.rs` do the I/O and report back through [`InboxModel::finish`].
//!
//! The list the hub returns (client-api.md §5.2) is the *server* state. What the user sees is the
//! server state with the in-flight operations laid over it, so a rollback is just dropping the
//! operation: the title, the row and the interrupt come back as they were.

use std::sync::Arc;
use std::time::Duration;

use client_core::outbox::new_idempotency_key;
use serde_json::{Value, json};

/// How long the search box must be quiet before the list is asked again.
pub const SEARCH_DEBOUNCE: Duration = Duration::from_millis(300);

/// Time source, injected so tests control the debounce.
pub trait Clock: Send + Sync {
    /// Time since an arbitrary fixed start; only differences matter.
    fn now(&self) -> Duration;
}

/// The wall clock.
pub struct SystemClock(std::time::Instant);

impl SystemClock {
    pub fn new() -> Self {
        Self(std::time::Instant::now())
    }
}

impl Default for SystemClock {
    fn default() -> Self {
        Self::new()
    }
}

impl Clock for SystemClock {
    fn now(&self) -> Duration {
        self.0.elapsed()
    }
}

/// The index status of a thread (client-api.md §5.2).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Status {
    Idle,
    Running,
    NeedsYou,
    Done,
    /// The last run failed (client-api.md §5.2).
    Failed,
    /// The last run was cancelled (client-api.md §5.2).
    Cancelled,
}

impl Status {
    /// Unknown strings are `Idle`, which sits with Done like the other finished states.
    pub fn parse(text: &str) -> Self {
        match text {
            "running" => Self::Running,
            "needs_you" => Self::NeedsYou,
            "done" => Self::Done,
            "failed" => Self::Failed,
            "cancelled" => Self::Cancelled,
            _ => Self::Idle,
        }
    }

    /// The `status=` query value.
    pub fn as_query(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Running => "running",
            Self::NeedsYou => "needs_you",
            Self::Done => "done",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }
}

/// The three inbox sections. `idle` threads (nothing started), failed and cancelled runs sit with
/// Done: nothing is waiting on the user and nothing is running.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Section {
    NeedsYou,
    Running,
    Done,
}

impl Section {
    pub const ALL: [Section; 3] = [Self::NeedsYou, Self::Running, Self::Done];

    pub fn slug(self) -> &'static str {
        match self {
            Self::NeedsYou => "needs-you",
            Self::Running => "running",
            Self::Done => "done",
        }
    }

    pub fn of(status: Status) -> Self {
        match status {
            Status::NeedsYou => Self::NeedsYou,
            Status::Running => Self::Running,
            Status::Done | Status::Failed | Status::Cancelled | Status::Idle => Self::Done,
        }
    }
}

/// One answer choice of an interrupt, exactly as the agent offered it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InterruptOption {
    pub option_id: String,
    pub name: String,
    /// `allow_once`, `allow_always`, `reject_once`, `reject_always`.
    pub kind: String,
}

/// What an open interrupt asks for (client-api.md §5.2, `pending_interrupts[].kind`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    /// An ACP permission request, answered with one of its own options.
    Approval,
    /// A form the user fills in on the thread. It has no options.
    Form,
    /// An auth request the user completes on the thread. It has no options.
    Auth,
    /// A kind this client does not know. It is never quick-answered.
    Other,
}

impl InterruptKind {
    /// The wire `kind`. A missing kind is an approval: rows stored before forms and auth were listed
    /// held only approvals.
    pub fn parse(kind: Option<&str>) -> Self {
        match kind {
            None | Some("approval") => Self::Approval,
            Some("form") => Self::Form,
            Some("auth") => Self::Auth,
            Some(_) => Self::Other,
        }
    }
}

/// An open interrupt on a thread, as the list reports it (`pending_interrupts`, when present).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PendingInterrupt {
    pub interrupt_id: String,
    pub title: String,
    pub kind: InterruptKind,
    pub options: Vec<InterruptOption>,
}

/// Which way a quick answer goes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Verdict {
    Allow,
    Deny,
}

impl PendingInterrupt {
    /// The interrupt's own option for `verdict`: the first `allow_*` / `reject_*` kind. `None`
    /// when it offers none (a form, say), in which case there is no quick answer.
    pub fn option_for(&self, verdict: Verdict) -> Option<&InterruptOption> {
        let prefix = match verdict {
            Verdict::Allow => "allow",
            Verdict::Deny => "reject",
        };
        self.options
            .iter()
            .find(|option| option.kind.starts_with(prefix))
    }
}

/// One row of `GET /v1/threads`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    pub thread_id: String,
    pub title: String,
    pub machine_id: String,
    pub agent: String,
    pub status: Status,
    pub updated_at: String,
    pub last_seq: u64,
    /// Present when the hub reports it; absent rows show no workspace.
    pub workspace: Option<String>,
    pub archived: bool,
    /// Every open interrupt of the thread, oldest first.
    pub pending: Vec<PendingInterrupt>,
}

impl ThreadRow {
    /// The first open approval. Quick Allow / Deny answer this one, whatever else is listed
    /// before it.
    pub fn first_approval(&self) -> Option<&PendingInterrupt> {
        self.pending
            .iter()
            .find(|interrupt| interrupt.kind == InterruptKind::Approval)
    }

    /// The row's detail line: the title of the first open interrupt of any kind.
    pub fn detail(&self) -> Option<&str> {
        self.pending
            .first()
            .map(|interrupt| interrupt.title.as_str())
    }
}

fn text_of(value: &Value, key: &str) -> String {
    value
        .get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn parse_option(option: &Value) -> Option<InterruptOption> {
    Some(InterruptOption {
        option_id: option.get("option_id")?.as_str()?.to_owned(),
        name: text_of(option, "name"),
        kind: text_of(option, "kind"),
    })
}

fn parse_interrupt(item: &Value) -> Option<PendingInterrupt> {
    Some(PendingInterrupt {
        interrupt_id: item.get("interrupt_id")?.as_str()?.to_owned(),
        title: text_of(item, "title"),
        kind: InterruptKind::parse(item.get("kind").and_then(Value::as_str)),
        options: item
            .get("options")
            .and_then(Value::as_array)
            .map(|options| options.iter().filter_map(parse_option).collect())
            .unwrap_or_default(),
    })
}

fn parse_row(thread: &Value) -> Option<ThreadRow> {
    Some(ThreadRow {
        thread_id: thread.get("thread_id")?.as_str()?.to_owned(),
        title: text_of(thread, "title"),
        machine_id: text_of(thread, "machine_id"),
        agent: text_of(thread, "agent"),
        status: Status::parse(&text_of(thread, "status")),
        updated_at: text_of(thread, "updated_at"),
        last_seq: thread.get("last_seq").and_then(Value::as_u64).unwrap_or(0),
        workspace: thread
            .get("workspace")
            .and_then(|w| w.as_str().or_else(|| w.get("path")?.as_str()))
            .map(str::to_owned),
        archived: thread
            .get("archived")
            .and_then(Value::as_bool)
            .unwrap_or(false),
        pending: thread
            .get("pending_interrupts")
            .and_then(Value::as_array)
            .map(|items| items.iter().filter_map(parse_interrupt).collect())
            .unwrap_or_default(),
    })
}

/// Parse the `GET /v1/threads` answer. Rows without a `thread_id` are skipped; unknown fields are
/// ignored. Returns the rows and the next cursor.
pub fn parse_threads(body: &Value) -> (Vec<ThreadRow>, Option<String>) {
    let rows = body
        .get("threads")
        .and_then(Value::as_array)
        .map(|threads| threads.iter().filter_map(parse_row).collect())
        .unwrap_or_default();
    let cursor = body
        .get("next_cursor")
        .and_then(Value::as_str)
        .map(str::to_owned);
    (rows, cursor)
}

/// The filters of the inbox (plan §3: status / agent / workspace; archived is shown on request).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Filters {
    pub status: Option<Status>,
    pub agent: Option<String>,
    pub workspace: String,
    pub show_archived: bool,
}

/// A `GET /v1/threads` request: the query pairs, and the generation that tells a late answer to
/// an older request from the current one.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ListRequest {
    pub generation: u64,
    pub query: Vec<(String, String)>,
}

/// A `PATCH /v1/threads/{id}` request (client-api.md §5.2: `{title?, archived?}`).
#[derive(Debug, Clone, PartialEq)]
pub struct PatchRequest {
    pub op: u64,
    pub thread_id: String,
    pub body: Value,
}

/// A `POST /v1/threads/{id}/interrupts/{interrupt_id}` request. The key is made once per intent.
#[derive(Debug, Clone, PartialEq)]
pub struct AnswerRequest {
    pub op: u64,
    pub thread_id: String,
    pub interrupt_id: String,
    pub idempotency_key: String,
    pub body: Value,
}

/// Why a request failed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Failure {
    pub status: Option<u16>,
    pub message: String,
}

#[derive(Debug, Clone)]
enum OpKind {
    Rename {
        thread_id: String,
        title: String,
    },
    Archive {
        thread_id: String,
        archived: bool,
    },
    Answer {
        thread_id: String,
        interrupt_id: String,
    },
}

impl OpKind {
    /// Lay this operation over `rows`.
    fn apply(&self, rows: &mut [ThreadRow]) {
        match self {
            Self::Rename { thread_id, title } => {
                if let Some(row) = rows.iter_mut().find(|r| &r.thread_id == thread_id) {
                    row.title.clone_from(title);
                }
            }
            Self::Archive {
                thread_id,
                archived,
            } => {
                if let Some(row) = rows.iter_mut().find(|r| &r.thread_id == thread_id) {
                    row.archived = *archived;
                }
            }
            Self::Answer {
                thread_id,
                interrupt_id,
            } => {
                if let Some(row) = rows.iter_mut().find(|r| &r.thread_id == thread_id) {
                    row.pending.retain(|p| &p.interrupt_id != interrupt_id);
                    if row.pending.is_empty() && row.status == Status::NeedsYou {
                        row.status = Status::Running;
                    }
                }
            }
        }
    }
}

#[derive(Debug, Clone)]
struct Op {
    id: u64,
    kind: OpKind,
}

/// One entry of the flattened, virtualizable list.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Item {
    Header {
        section: Section,
        count: usize,
    },
    /// Index into [`InboxModel::rows`].
    Row(usize),
}

pub struct InboxModel {
    clock: Arc<dyn Clock>,
    server: Vec<ThreadRow>,
    shown: Vec<ThreadRow>,
    filters: Filters,
    search_text: String,
    search_applied: String,
    search_changed_at: Option<Duration>,
    generation: u64,
    ops: Vec<Op>,
    next_op: u64,
    error: Option<String>,
}

impl InboxModel {
    pub fn new(clock: Arc<dyn Clock>) -> Self {
        Self {
            clock,
            server: Vec::new(),
            shown: Vec::new(),
            filters: Filters::default(),
            search_text: String::new(),
            search_applied: String::new(),
            search_changed_at: None,
            generation: 0,
            ops: Vec::new(),
            next_op: 1,
            error: None,
        }
    }

    // ---- reading -------------------------------------------------------------------------

    /// What the user sees: the server rows with in-flight operations applied.
    pub fn rows(&self) -> &[ThreadRow] {
        &self.shown
    }

    pub fn row(&self, thread_id: &str) -> Option<&ThreadRow> {
        self.shown.iter().find(|row| row.thread_id == thread_id)
    }

    pub fn filters(&self) -> &Filters {
        &self.filters
    }

    pub fn search_text(&self) -> &str {
        &self.search_text
    }

    /// The last failure, shown until the next successful operation or `clear_error`.
    pub fn error(&self) -> Option<&str> {
        self.error.as_deref()
    }

    pub fn clear_error(&mut self) {
        self.error = None;
    }

    /// Keep a failure that did not come from an operation (the list could not be fetched).
    pub fn report(&mut self, failure: Failure) {
        self.error = Some(failure.message);
    }

    /// The generation of the latest [`Self::list_request`].
    pub fn generation(&self) -> u64 {
        self.generation
    }

    /// Operations that have been started and not finished.
    pub fn in_flight(&self) -> usize {
        self.ops.len()
    }

    /// Rows of one section, in the hub's order (newest first).
    pub fn section(&self, section: Section) -> Vec<&ThreadRow> {
        self.shown
            .iter()
            .filter(|row| Section::of(row.status) == section)
            .collect()
    }

    /// The flat list the view virtualizes: a header per non-empty section, then its rows.
    pub fn items(&self) -> Vec<Item> {
        let mut items = Vec::new();
        for section in Section::ALL {
            let indices: Vec<usize> = self
                .shown
                .iter()
                .enumerate()
                .filter(|(_, row)| Section::of(row.status) == section)
                .map(|(ix, _)| ix)
                .collect();
            if indices.is_empty() {
                continue;
            }
            items.push(Item::Header {
                section,
                count: indices.len(),
            });
            items.extend(indices.into_iter().map(Item::Row));
        }
        items
    }

    // ---- listing -------------------------------------------------------------------------

    /// The request that fetches the list for the current search and filters. Every call starts a
    /// new generation, so answers to earlier requests are ignored by [`Self::apply_list`].
    pub fn list_request(&mut self) -> ListRequest {
        self.generation += 1;
        let mut query = Vec::new();
        if let Some(status) = self.filters.status {
            query.push(("status".to_owned(), status.as_query().to_owned()));
        }
        if let Some(agent) = &self.filters.agent {
            query.push(("agent".to_owned(), agent.clone()));
        }
        let workspace = self.filters.workspace.trim();
        if !workspace.is_empty() {
            query.push(("workspace".to_owned(), workspace.to_owned()));
        }
        let q = self.search_applied.trim();
        if !q.is_empty() {
            query.push(("q".to_owned(), q.to_owned()));
        }
        if self.filters.show_archived {
            query.push(("archived".to_owned(), "true".to_owned()));
        }
        ListRequest {
            generation: self.generation,
            query,
        }
    }

    /// Take the rows of the answer to request `generation`. A late answer to an older request is
    /// dropped (returns `false`).
    pub fn apply_list(&mut self, generation: u64, rows: Vec<ThreadRow>) -> bool {
        if generation != self.generation {
            return false;
        }
        self.server = rows;
        self.recompute();
        true
    }

    /// The setters return whether the filters changed (the list must be fetched again).
    pub fn set_status_filter(&mut self, status: Option<Status>) -> bool {
        self.change_filters(|filters| filters.status = status)
    }

    pub fn set_agent_filter(&mut self, agent: Option<&str>) -> bool {
        self.change_filters(|filters| filters.agent = agent.map(str::to_owned))
    }

    pub fn set_workspace_filter(&mut self, workspace: &str) -> bool {
        self.change_filters(|filters| workspace.clone_into(&mut filters.workspace))
    }

    pub fn set_show_archived(&mut self, show: bool) -> bool {
        self.change_filters(|filters| filters.show_archived = show)
    }

    fn change_filters(&mut self, change: impl FnOnce(&mut Filters)) -> bool {
        let before = self.filters.clone();
        change(&mut self.filters);
        let changed = self.filters != before;
        if changed {
            self.recompute();
        }
        changed
    }

    // ---- search --------------------------------------------------------------------------

    /// The search box changed. The list is not asked again until [`Self::poll_search`] says the
    /// box has been quiet for [`SEARCH_DEBOUNCE`].
    pub fn set_search(&mut self, text: &str) {
        if self.search_text != text {
            text.clone_into(&mut self.search_text);
            self.search_changed_at = Some(self.clock.now());
        }
    }

    /// Whether a search is waiting for its debounce to pass.
    pub fn search_pending(&self) -> bool {
        self.search_changed_at.is_some()
    }

    /// `true` once, when the debounce has passed and the text differs from the one last used:
    /// the caller then fetches [`Self::list_request`].
    pub fn poll_search(&mut self) -> bool {
        let Some(changed_at) = self.search_changed_at else {
            return false;
        };
        if self.clock.now().saturating_sub(changed_at) < SEARCH_DEBOUNCE {
            return false;
        }
        self.search_changed_at = None;
        if self.search_applied == self.search_text {
            return false;
        }
        self.search_applied.clone_from(&self.search_text);
        true
    }

    // ---- operations ----------------------------------------------------------------------

    fn start(&mut self, kind: OpKind) -> u64 {
        let id = self.next_op;
        self.next_op += 1;
        self.ops.push(Op { id, kind });
        self.recompute();
        id
    }

    /// Rename a thread. The new title shows at once; `None` when it is blank or unchanged.
    pub fn begin_rename(&mut self, thread_id: &str, title: &str) -> Option<PatchRequest> {
        let title = title.trim();
        let row = self.row(thread_id)?;
        if title.is_empty() || row.title == title {
            return None;
        }
        let title = title.to_owned();
        let op = self.start(OpKind::Rename {
            thread_id: thread_id.to_owned(),
            title: title.clone(),
        });
        Some(PatchRequest {
            op,
            thread_id: thread_id.to_owned(),
            body: json!({ "title": title }),
        })
    }

    /// Archive (or restore) a thread. The row leaves the list at once unless archived threads are
    /// shown; `None` for an unknown thread.
    pub fn begin_archive(&mut self, thread_id: &str, archived: bool) -> Option<PatchRequest> {
        self.row(thread_id)?;
        let op = self.start(OpKind::Archive {
            thread_id: thread_id.to_owned(),
            archived,
        });
        Some(PatchRequest {
            op,
            thread_id: thread_id.to_owned(),
            body: json!({ "archived": archived }),
        })
    }

    /// Answer the thread's first open approval with the interrupt's own allow / reject option.
    /// Forms and auth in the list are skipped. `None` when the thread has no approval, or its first
    /// approval offers no such option. An approval that is already being answered is hidden from
    /// the row, so it cannot be answered twice.
    pub fn begin_answer(&mut self, thread_id: &str, verdict: Verdict) -> Option<AnswerRequest> {
        let interrupt = self.row(thread_id)?.first_approval()?.clone();
        let option_id = interrupt.option_for(verdict)?.option_id.clone();
        let op = self.start(OpKind::Answer {
            thread_id: thread_id.to_owned(),
            interrupt_id: interrupt.interrupt_id.clone(),
        });
        Some(AnswerRequest {
            op,
            thread_id: thread_id.to_owned(),
            interrupt_id: interrupt.interrupt_id,
            idempotency_key: new_idempotency_key(),
            body: json!({ "outcome": { "type": "selected", "option_id": option_id } }),
        })
    }

    /// The hub answered operation `op`. On success the server state takes the change; on failure
    /// the change is rolled back and the error is kept for the view. An answer the hub refuses
    /// because somebody else already resolved it or it expired (409 / 410) drops the approval: it
    /// is not open any more.
    pub fn finish(&mut self, op: u64, result: Result<(), Failure>) {
        let Some(position) = self.ops.iter().position(|o| o.id == op) else {
            return;
        };
        let Op { kind, .. } = self.ops.remove(position);
        match result {
            Ok(()) => {
                self.error = None;
                kind.apply(&mut self.server);
            }
            Err(failure) => {
                if matches!(kind, OpKind::Answer { .. })
                    && matches!(failure.status, Some(409 | 410))
                {
                    kind.apply(&mut self.server);
                }
                self.error = Some(failure.message);
            }
        }
        self.recompute();
    }

    /// Rebuild what is shown: server rows, in-flight operations on top, archived threads left out
    /// unless asked for.
    fn recompute(&mut self) {
        let mut rows = self.server.clone();
        for op in &self.ops {
            op.kind.apply(&mut rows);
        }
        if !self.filters.show_archived {
            rows.retain(|row| !row.archived);
        }
        self.shown = rows;
    }
}
