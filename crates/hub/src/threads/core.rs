//! `ThreadCore`: the runtime-agnostic logic of one thread (M2-008). No worker types, no IO, no
//! clock (callers pass `now_ms`); storage sits behind [`ThreadStore`]. The Thread Durable Object
//! (`do/thread.rs`) runs this code, and native unit tests drive it with a
//! [`MemoryStore`](super::MemoryStore).
//!
//! What it owns:
//!
//! - the append path: `seq` stamping ([`ThreadSeq`], turn-event.md §3), the §5 ordering rules
//!   ([`SequenceValidator`], rebuilt from the stored log on [`ThreadCore::open`]), per-scope
//!   state (`StateSnapshot` / `StateDelta` applied as JSON Patch), run and interrupt
//!   bookkeeping;
//! - the subscription catch-up (§6): `hello`, `snapshot` as of `R - 1`, replay from the run
//!   boundary, `live`;
//! - the client-api.md writes: prompts and the pending-task queue (§5.3), interrupt answers
//!   (§5.4), cancel (§5.5) and idempotency (§3), emitting the events of §6 ("who emits what")
//!   and queueing tunnel commands in an outbox the Machine DO drains.
//!
//! Every mutating method is synchronous and all-or-nothing as far as the caller can observe:
//! the Durable Object does not yield between its storage writes, so a request's writes commit
//! together.

use protocol::mapping::acp_turn::SequenceValidator;
use protocol::tunnel::{Actor, Command, ContentPart, Outcome, Workspace};
use protocol::turn_event::{
    Envelope, Event, InterruptKind, InterruptResolved, MAX_ENVELOPE_BYTES, PatchOp, PatchOpKind,
    ResolvedOutcome, RunError, RunErrorReason, RunOutcome, RunReason, RunStarted, StateDelta,
    StateScope, StateSnapshot, ThreadSeq, UserMessage, format_timestamp, is_valid_id,
};
use serde::Deserialize;
use serde_json::{Map, Value, json};
use std::collections::VecDeque;

use super::error::ApiError;
use super::patch::apply_patch;
use super::redact::redact_envelope;
use super::store::{
    CommandRecord, IdempotencyRecord, InterruptRecord, MemoryStore, Resolution, RunRecord,
    TaskRecord, TaskStatus, ThreadStore,
};
use super::timestamp::parse_timestamp;
use crate::tasks::index::{IndexRow, STATUS_DONE, STATUS_OPEN, STATUS_RUNNING, STATUS_WAITING};
use crate::tasks::title::derive_title;

/// The longest title a client may set (`POST /v1/threads`, `PATCH /v1/threads/{id}`).
const MAX_TITLE_CHARS: usize = 200;

/// `PATCH /v1/threads/{id}` body (client-api.md §5.2): a new title and/or the archive flag.
#[derive(Debug, Clone, PartialEq, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct UpdateThreadBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub archived: Option<bool>,
}

/// How long an `Idempotency-Key` answer is kept (client-api.md §3).
pub const IDEMPOTENCY_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// How long an appended `event_id` is remembered (tunnel.md §4.2): a retransmit inside this
/// window is acknowledged without a second append.
pub const EVENT_ID_TTL_MS: u64 = 24 * 60 * 60 * 1000;

/// Characters of the first text part kept as a pending task's `preview`.
const PREVIEW_CHARS: usize = 80;

/// Stored envelopes read per chunk when rebuilding the validator.
const REBUILD_CHUNK: u64 = 500;

/// Stored envelopes read per page by the catch-up (turn-event.md §6): a replay holds at most
/// this many rows at a time, however long the thread is.
pub const CATCH_UP_PAGE: u64 = 500;

/// How many of the newest runs keep their state history addressable by default. Older state
/// changes are compacted (see [`ThreadCore::with_state_retention`]).
pub const RETAINED_RUNS: usize = 50;

/// The meta key of the state floor: the seq the state history is compacted to (0 = not compacted).
const STATE_FLOOR_KEY: &str = "state_floor";

/// The four state scopes, in the order of the `snapshot` message.
pub const SCOPES: [StateScope; 4] = [
    StateScope::Session,
    StateScope::Usage,
    StateScope::Thread,
    StateScope::Flow,
];

/// The JSON name of a state scope (`session`, `usage`, `thread`, `flow`).
pub fn scope_name(scope: StateScope) -> &'static str {
    match scope {
        StateScope::Session => "session",
        StateScope::Usage => "usage",
        StateScope::Thread => "thread",
        StateScope::Flow => "flow",
    }
}

/// An HTTP answer: status and JSON body (`Null` = no body, for 204).
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub status: u16,
    pub body: Value,
}

impl Reply {
    pub fn new(status: u16, body: Value) -> Self {
        Self { status, body }
    }
}

/// An accepted interrupt answer.
#[derive(Debug, Clone, PartialEq)]
pub struct Resolved {
    /// The stamped `InterruptResolved` envelope, already in the log.
    pub envelope: Envelope,
    /// The task the interrupt belongs to.
    pub task_id: String,
}

/// `POST /v1/threads/{id}/prompts` body (client-api.md §5.3).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct PromptBody {
    #[serde(default)]
    pub task_id: Option<String>,
    #[serde(default)]
    pub message_id: Option<String>,
    pub parts: Vec<ContentPart>,
}

/// `POST /v1/threads/{id}/session` body (client-api.md §5.5a): a mode, a config option, or both.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionBody {
    #[serde(default)]
    pub mode_id: Option<String>,
    #[serde(default)]
    pub config: Option<SessionConfigBody>,
}

/// The `config` of [`SessionBody`]: an option id and its new value (a value id, or a boolean).
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct SessionConfigBody {
    pub id: String,
    pub value: Value,
}

/// `POST /v1/threads` body (client-api.md §5.2), as the Worker forwards it to the Thread DO.
#[derive(Debug, Clone, PartialEq, Deserialize)]
pub struct CreateThreadBody {
    pub machine_id: String,
    pub agent: String,
    pub workspace: Workspace,
    #[serde(default)]
    pub permission_mode: Option<String>,
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub prompt: Option<PromptBody>,
}

/// The logic of one thread over a [`ThreadStore`].
pub struct ThreadCore<S: ThreadStore> {
    thread_id: String,
    store: S,
    validator: SequenceValidator,
    head: u64,
    /// Everything appended since the last [`ThreadCore::take_appended`], for broadcasting.
    outbox: Vec<Envelope>,
    /// `None`: never compact the state history.
    retained_runs: Option<usize>,
}

impl<S: ThreadStore> std::fmt::Debug for ThreadCore<S> {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ThreadCore")
            .field("thread_id", &self.thread_id)
            .field("head", &self.head)
            .finish_non_exhaustive()
    }
}

impl ThreadCore<MemoryStore> {
    /// A new, empty in-memory thread (unlike [`ThreadCore::open`], it cannot fail).
    pub fn new_in_memory(thread_id: impl Into<String>) -> Self {
        Self {
            thread_id: thread_id.into(),
            store: MemoryStore::new(),
            validator: SequenceValidator::new(),
            head: 0,
            outbox: Vec::new(),
            retained_runs: Some(RETAINED_RUNS),
        }
    }
}

/// The messages of one subscription (turn-event.md §6), read lazily: `hello` and `snapshot` first,
/// then the stored events in pages of [`CATCH_UP_PAGE`] rows, then `live`. It holds its own handle
/// to the store, so it can be drained after the core is released (the Durable Object sends each
/// message as it is produced; nothing is appended in between).
pub struct CatchUp<S: ThreadStore> {
    store: S,
    head: u64,
    next: u64,
    pending: VecDeque<Value>,
    done: bool,
}

impl<S: ThreadStore> Iterator for CatchUp<S> {
    type Item = Value;

    fn next(&mut self) -> Option<Value> {
        loop {
            if let Some(message) = self.pending.pop_front() {
                return Some(message);
            }
            if self.done {
                return None;
            }
            if self.next > self.head {
                self.done = true;
                return Some(json!({ "type": "live" }));
            }
            let to = (self.next + CATCH_UP_PAGE - 1).min(self.head);
            match self.store.read_range(self.next, to) {
                Ok(page) => {
                    self.pending.extend(page.iter().map(event_message));
                    self.next = to + 1;
                }
                Err(_) => {
                    // The replay stops here; the subscriber reconnects with `?after=`.
                    self.done = true;
                    return Some(json!({
                        "type": "error",
                        "code": "internal",
                        "message": "replay stopped; reconnect with after=<last seq>"
                    }));
                }
            }
        }
    }
}

impl<S: ThreadStore> ThreadCore<S> {
    /// Opens the thread over `store`, rebuilding the §5 validator from the stored log.
    pub fn open(thread_id: impl Into<String>, store: S) -> Result<Self, ApiError> {
        let head = store.head_seq()?;
        let mut core = Self {
            thread_id: thread_id.into(),
            store,
            validator: SequenceValidator::new(),
            head,
            outbox: Vec::new(),
            retained_runs: Some(RETAINED_RUNS),
        };
        core.rebuild_validator()?;
        Ok(core)
    }

    /// Sets how many of the newest runs keep their state history addressable (`None`: keep all).
    /// The default is [`RETAINED_RUNS`]; at least 1.
    pub fn with_state_retention(mut self, retained_runs: Option<usize>) -> Self {
        self.retained_runs = retained_runs.map(|runs| runs.max(1));
        self
    }

    pub fn thread_id(&self) -> &str {
        &self.thread_id
    }

    /// The last stored `seq` (0 when empty).
    pub fn head_seq(&self) -> u64 {
        self.head
    }

    pub fn store(&self) -> &S {
        &self.store
    }

    /// Appends one envelope unless its `event_id` was appended within the last 24 hours
    /// (tunnel.md §4.2). `Ok(None)` is the acknowledged duplicate: nothing is appended. The id
    /// is remembered only after the append succeeded, so a failed append is retried.
    pub fn append_once(
        &mut self,
        event_id: &str,
        envelope: Envelope,
        now_ms: u64,
    ) -> Result<Option<Envelope>, ApiError> {
        if self.store.event_seen(event_id, now_ms)? {
            return Ok(None);
        }
        let stored = self.append_inner(envelope, now_ms)?;
        self.store.record_event(event_id, now_ms)?;
        Ok(Some(stored))
    }

    /// Envelopes appended since the last call, in order. The caller broadcasts them to the
    /// subscribers; call it after every operation, successful or not (a failed multi-event
    /// operation may have appended some).
    pub fn take_appended(&mut self) -> Vec<Envelope> {
        std::mem::take(&mut self.outbox)
    }

    fn rebuild_validator(&mut self) -> Result<(), ApiError> {
        let mut validator = SequenceValidator::new();
        let mut from = 1;
        while from <= self.head {
            let to = (from + REBUILD_CHUNK - 1).min(self.head);
            for envelope in self.store.read_range(from, to)? {
                validator.push(&envelope).map_err(|error| {
                    ApiError::internal(format!("the stored log violates turn-event.md §5: {error}"))
                })?;
            }
            from = to + 1;
        }
        self.validator = validator;
        Ok(())
    }

    // ---- append path ------------------------------------------------------------------------

    /// Appends one producer envelope (`seq: 0`): size check, `seq` stamping, §5 validation and
    /// state application. Errors leave the log unchanged (`422 invalid_sequence`). When the
    /// envelope ends the running task, the next pending task starts (client-api.md §5.3), so
    /// more than one envelope may be appended; see [`ThreadCore::take_appended`].
    pub fn append(&mut self, envelope: Envelope, now_ms: u64) -> Result<Envelope, ApiError> {
        self.append_inner(envelope, now_ms)
    }

    fn append_inner(&mut self, mut envelope: Envelope, now_ms: u64) -> Result<Envelope, ApiError> {
        if envelope.thread_id != self.thread_id {
            return Err(ApiError::invalid_request(format!(
                "envelope thread_id {} is not this thread ({})",
                envelope.thread_id, self.thread_id
            )));
        }
        // Secrets never reach the log, the state or the subscribers (turn-event.md §2). Before
        // the size check: the limit applies to what is stored.
        redact_envelope(&mut envelope)?;
        let size = serde_json::to_vec(&envelope)
            .map_err(|error| ApiError::internal(error.to_string()))?
            .len();
        if size > MAX_ENVELOPE_BYTES {
            return Err(ApiError::new(
                413,
                "too_large",
                format!("envelope is {size} bytes, the limit is {MAX_ENVELOPE_BYTES}"),
            ));
        }
        let state_change = self.state_change(&envelope)?;
        let envelope = ThreadSeq::resume(self.head)
            .stamp(envelope)
            .map_err(|error| ApiError::invalid_sequence(error.to_string()))?;
        if let Err(error) = self.validator.push(&envelope) {
            // `push` may have advanced part of its state before failing: start over from the log.
            self.rebuild_validator()?;
            return Err(ApiError::invalid_sequence(error.to_string()));
        }
        if let Err(error) = self.persist(&envelope, state_change) {
            // Best effort: realign the validator with what was stored.
            let _ = self.rebuild_validator();
            return Err(error);
        }
        self.outbox.push(envelope.clone());
        if ends_task(&envelope.event) {
            self.start_next_pending(now_ms)?;
        }
        Ok(envelope)
    }

    fn persist(
        &mut self,
        envelope: &Envelope,
        state_change: Option<(StateScope, Value)>,
    ) -> Result<(), ApiError> {
        self.store.append_event(envelope)?;
        self.head = envelope.seq;
        if let Some((scope, state)) = state_change {
            self.store.put_state(scope, envelope.seq, &state)?;
        }
        self.observe(envelope)
    }

    /// The new state of a scope when `envelope` is a `StateSnapshot` / `StateDelta`.
    fn state_change(&self, envelope: &Envelope) -> Result<Option<(StateScope, Value)>, ApiError> {
        match &envelope.event {
            Event::StateSnapshot(snapshot) => Ok(Some((
                snapshot.scope,
                Value::Object(snapshot.snapshot.clone()),
            ))),
            Event::StateDelta(delta) => {
                let mut state = self
                    .store
                    .state_at(delta.scope, self.head)?
                    .unwrap_or_else(|| json!({}));
                apply_patch(&mut state, &delta.patch).map_err(|message| {
                    ApiError::invalid_sequence(format!("StateDelta does not apply: {message}"))
                })?;
                Ok(Some((delta.scope, state)))
            }
            _ => Ok(None),
        }
    }

    /// Updates task, run and interrupt bookkeeping for a stored envelope.
    fn observe(&mut self, envelope: &Envelope) -> Result<(), ApiError> {
        let task_id = envelope.task_id.clone().unwrap_or_default();
        match &envelope.event {
            Event::UserMessage(message) => {
                let mut task = self
                    .store
                    .get_task(&task_id)?
                    .unwrap_or_else(|| TaskRecord {
                        task_id: task_id.clone(),
                        status: TaskStatus::Running,
                        message_id: message.message_id.clone(),
                        parts: message.parts.clone(),
                        author: message.author.clone(),
                        preview: preview_of(&message.parts),
                        queued_at: envelope.ts.clone(),
                        idempotency_key: String::new(),
                        has_run: false,
                        cancel_requested: false,
                    });
                task.status = TaskStatus::Running;
                self.store.put_task(&task)?;
                // A new prompt starts the thread again: it is no longer `done`.
                self.store.put_meta("finished_ok", &json!(false))?;
                // The auto-title comes from the first prompt (tasks::title); it is kept once.
                let first_text = message.parts.iter().find_map(|part| match part {
                    ContentPart::Text { text } => Some(text.as_str()),
                    ContentPart::File { .. } => None,
                });
                if self.store.get_meta("derived_title")?.is_none()
                    && let Some(title) = first_text.and_then(derive_title)
                {
                    self.store.put_meta("derived_title", &json!(title))?;
                }
            }
            Event::RunStarted(_) => {
                if let Some(run_id) = &envelope.run_id {
                    self.store.put_run(&RunRecord {
                        run_id: run_id.clone(),
                        task_id: task_id.clone(),
                        start_seq: envelope.seq,
                        end_seq: None,
                    })?;
                }
                if let Some(mut task) = self.store.get_task(&task_id)? {
                    task.has_run = true;
                    self.store.put_task(&task)?;
                }
                self.compact_state_history()?;
            }
            Event::RunFinished(_) | Event::RunError(_) => {
                self.end_run(envelope)?;
                if ends_task(&envelope.event)
                    && let Some(mut task) = self.store.get_task(&task_id)?
                {
                    task.status = TaskStatus::Finished;
                    self.store.put_task(&task)?;
                }
                // `done` is a run that finished with success; an error leaves the thread idle.
                let succeeded =
                    matches!(envelope.event, Event::RunFinished(_)) && ends_task(&envelope.event);
                self.store.put_meta("finished_ok", &json!(succeeded))?;
            }
            Event::Interrupt(interrupt) => {
                self.store.put_interrupt(&InterruptRecord {
                    interrupt_id: interrupt.interrupt_id.clone(),
                    task_id,
                    kind: interrupt.kind,
                    option_ids: interrupt
                        .options
                        .iter()
                        .flatten()
                        .map(|option| option.option_id.clone())
                        .collect(),
                    expires_at: interrupt.expires_at.clone(),
                    resolved: None,
                })?;
            }
            Event::InterruptResolved(resolved) => {
                if let Some(mut record) = self.store.get_interrupt(&resolved.interrupt_id)? {
                    record.resolved = Some(Resolution {
                        resolved_by: resolved.resolved_by.clone(),
                        outcome: resolved.outcome.clone(),
                    });
                    self.store.put_interrupt(&record)?;
                }
            }
            _ => {}
        }
        Ok(())
    }

    fn end_run(&mut self, envelope: &Envelope) -> Result<(), ApiError> {
        if let Some(run_id) = &envelope.run_id
            && let Some(mut run) = self.store.get_run(run_id)?
        {
            run.end_seq = Some(envelope.seq);
            self.store.put_run(&run)?;
        }
        Ok(())
    }

    // ---- subscription (turn-event.md §6) ----------------------------------------------------

    /// `R = min(boundary, after + 1)`, where the boundary is the `RunStarted` seq of the run
    /// open at `after` (§6 step 1). `after` is clamped to the head.
    pub fn replay_from(&self, after: u64) -> Result<u64, ApiError> {
        // The state history before the state floor is compacted: catch-up never starts before it.
        let after = after.min(self.head).max(self.state_floor()?);
        let boundary = self.store.run_boundary_at(after)?.unwrap_or(after + 1);
        Ok(boundary.min(after + 1))
    }

    /// The state of every scope that has one, as of `seq`: `{"session": {...}, ...}`.
    pub fn states_at(&self, seq: u64) -> Result<Value, ApiError> {
        let mut states = Map::new();
        for scope in SCOPES {
            if let Some(state) = self.store.state_at(scope, seq)? {
                states.insert(scope_name(scope).to_owned(), state);
            }
        }
        Ok(Value::Object(states))
    }

    /// The state floor: the seq the state history is compacted to. `0` when nothing is compacted.
    pub fn state_floor(&self) -> Result<u64, ApiError> {
        Ok(self
            .store
            .get_meta(STATE_FLOOR_KEY)?
            .and_then(|floor| floor.as_u64())
            .unwrap_or(0))
    }

    /// Compacts the state history older than the oldest of the newest `retained_runs` runs: each
    /// scope keeps one snapshot (its latest change at or before the floor) and drops the older
    /// changes. Runs once per `RunStarted`; a no-op when there are fewer runs or no retention.
    fn compact_state_history(&mut self) -> Result<(), ApiError> {
        let Some(retained) = self.retained_runs else {
            return Ok(());
        };
        let back = u64::try_from(retained - 1).unwrap_or(u64::MAX);
        let Some(oldest) = self.store.run_start_back(back)? else {
            return Ok(());
        };
        let floor = oldest.saturating_sub(1);
        if floor <= self.state_floor()? {
            return Ok(());
        }
        for scope in SCOPES {
            self.store.compact_states(scope, floor)?;
        }
        self.store.put_meta(STATE_FLOOR_KEY, &json!(floor))?;
        Ok(())
    }
}

impl<S: ThreadStore + Clone> ThreadCore<S> {
    /// The messages a new subscriber gets for `?after=<after>`, as a lazy [`CatchUp`]: `hello`,
    /// `snapshot` (as of `R - 1`), every stored event with `seq >= R` (read in pages), then
    /// `live`. Built without reading the log, so the caller reads it after the core is released;
    /// it is safe because nothing is appended between this call and the drain.
    pub fn catch_up(&self, after: u64) -> Result<CatchUp<S>, ApiError> {
        self.ensure_live()?;
        let replay_from = self.replay_from(after)?;
        let hello = json!({ "type": "hello", "head_seq": self.head, "replay_from": replay_from });
        let snapshot = json!({
            "type": "snapshot",
            "at_seq": replay_from - 1,
            "states": self.states_at(replay_from - 1)?,
        });
        Ok(CatchUp {
            store: self.store.clone(),
            head: self.head,
            next: replay_from,
            pending: VecDeque::from([hello, snapshot]),
            done: false,
        })
    }
}

impl<S: ThreadStore> ThreadCore<S> {
    // ---- thread info ------------------------------------------------------------------------

    /// Stores what the hub knows about the thread (`machine_id`, `agent`, `workspace`,
    /// `permission_mode`, `title`; merged into earlier values) and publishes the `thread`
    /// state. Called by thread creation (internal hook, `POST /internal/threads/{id}/init`).
    pub fn init_thread(&mut self, info: &Map<String, Value>, now_ms: u64) -> Result<(), ApiError> {
        let mut merged = match self.store.get_meta("info")? {
            Some(Value::Object(existing)) => existing,
            _ => Map::new(),
        };
        for key in [
            "machine_id",
            "agent",
            "workspace",
            "permission_mode",
            "title",
        ] {
            if let Some(value) = info.get(key) {
                merged.insert(key.to_owned(), value.clone());
            }
        }
        self.store.put_meta("info", &Value::Object(merged))?;
        match self.store.state_at(StateScope::Thread, self.head)? {
            None => {
                let snapshot = self.new_thread_state()?;
                self.append_thread_event(
                    StateSnapshot {
                        scope: StateScope::Thread,
                        snapshot,
                    },
                    now_ms,
                )?;
            }
            Some(_) => {
                if let Some(title) = info.get("title") {
                    self.append_thread_event(
                        StateDelta {
                            scope: StateScope::Thread,
                            patch: vec![patch_add("/title", title.clone())],
                        },
                        now_ms,
                    )?;
                }
            }
        }
        Ok(())
    }

    /// `POST /v1/threads` (client-api.md §5.2), the Thread DO half. Records the host machine
    /// through the init hook ([`ThreadCore::init_thread`]) and, with a prompt, starts the first
    /// task (`UserMessage`, §6) and queues a `dispatch` command for the Machine DO (tunnel.md
    /// §3.5). 201 `{thread_id, task_id, status: "started"}`; without a prompt the thread is
    /// created idle (`task_id: null`). Replays under the same key are answered by [`ThreadCore::idempotent`].
    pub fn create_thread(
        &mut self,
        actor: &Actor,
        idempotency_key: &str,
        body: CreateThreadBody,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        if !is_valid_id(&body.machine_id) {
            return Err(ApiError::invalid_request(
                "machine_id must be 1-128 characters of [A-Za-z0-9_:.-]",
            ));
        }
        if body.agent.trim().is_empty() {
            return Err(ApiError::invalid_request("agent is required"));
        }
        if body.workspace.path.trim().is_empty() {
            return Err(ApiError::invalid_request("workspace.path is required"));
        }
        // Everything is checked before the first event or state write.
        let prompt = match body.prompt {
            None => None,
            Some(prompt) => {
                if prompt.parts.is_empty() {
                    return Err(ApiError::invalid_request(
                        "parts must have at least one part",
                    ));
                }
                let task_id = match prompt.task_id {
                    Some(id) => checked_id("task_id", id)?,
                    None => self.new_id("tk", now_ms)?,
                };
                let message_id = match prompt.message_id {
                    Some(id) => checked_id("message_id", id)?,
                    None => self.new_id("msg", now_ms)?,
                };
                Some((task_id, message_id, prompt.parts))
            }
        };
        if self.head > 0 || self.store.get_meta("info")?.is_some() {
            return Err(ApiError::new(
                409,
                "conflict",
                format!("thread {} already exists", self.thread_id),
            ));
        }

        let mut info = Map::new();
        info.insert("machine_id".to_owned(), json!(body.machine_id));
        info.insert("agent".to_owned(), json!(body.agent));
        info.insert("workspace".to_owned(), json!(body.workspace));
        let permission_mode = body.permission_mode.unwrap_or_else(|| "default".to_owned());
        info.insert("permission_mode".to_owned(), json!(permission_mode));
        if let Some(title) = body.title {
            info.insert("title".to_owned(), json!(title));
        }
        self.init_thread(&info, now_ms)?;

        let Some((task_id, message_id, parts)) = prompt else {
            return Ok(Reply::new(
                201,
                json!({ "thread_id": self.thread_id, "task_id": null, "status": "idle" }),
            ));
        };
        let task = TaskRecord {
            task_id: task_id.clone(),
            status: TaskStatus::Pending,
            message_id,
            preview: preview_of(&parts),
            parts,
            author: actor.clone(),
            queued_at: format_timestamp(now_ms),
            idempotency_key: idempotency_key.to_owned(),
            has_run: false,
            cancel_requested: false,
        };
        self.store.put_task(&task)?;
        self.begin_task(&task, now_ms)?;
        let command = Command::Dispatch {
            thread_id: self.thread_id.clone(),
            task_id: task_id.clone(),
            agent: body.agent,
            workspace: body.workspace,
            permission_mode,
            message_id: task.message_id.clone(),
            parts: task.parts.clone(),
            author: actor.clone(),
        };
        self.store
            .queue_command(idempotency_key, &format_timestamp(now_ms), &command)?;
        Ok(Reply::new(
            201,
            json!({ "thread_id": self.thread_id, "task_id": task_id, "status": "started" }),
        ))
    }

    /// `GET /v1/threads/{id}`; `404` for a thread nothing has happened to.
    pub fn thread_info(&self) -> Result<Reply, ApiError> {
        let info = match self.store.get_meta("info")? {
            Some(Value::Object(info)) => info,
            _ => Map::new(),
        };
        if info.is_empty() && self.head == 0 {
            return Err(ApiError::not_found(format!("no thread {}", self.thread_id)));
        }
        let states = self.states_at(self.head)?;
        let title = info
            .get("title")
            .or_else(|| states.pointer("/thread/title"))
            .cloned()
            .unwrap_or(Value::Null);
        Ok(Reply::new(
            200,
            json!({
                "thread_id": self.thread_id,
                "title": title,
                "machine_id": info.get("machine_id").cloned().unwrap_or(Value::Null),
                "agent": info.get("agent").cloned().unwrap_or(Value::Null),
                "workspace": info.get("workspace").cloned().unwrap_or(Value::Null),
                "permission_mode": info.get("permission_mode").cloned().unwrap_or(Value::Null),
                "head_seq": self.head,
                "states": states,
            }),
        ))
    }

    /// The machine that hosts the thread (set by `init_thread`), if known.
    pub fn machine_id(&self) -> Result<Option<String>, ApiError> {
        Ok(self.store.get_meta("info")?.and_then(|info| {
            info.get("machine_id")
                .and_then(Value::as_str)
                .map(str::to_owned)
        }))
    }

    // ---- titles, archive and delete (client-api.md §5.2, M2-010) ----------------------------

    /// `PATCH /v1/threads/{id}`: sets the title (the client's title wins over the auto-title)
    /// and/or the archive flag. A title change is also appended as `StateDelta{thread}` when the
    /// thread has state. 200 `{thread_id, title, archived}`; `400` for an empty body or a bad
    /// title, `404` for an unknown thread.
    pub fn update_thread(
        &mut self,
        body: UpdateThreadBody,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        if body.title.is_none() && body.archived.is_none() {
            return Err(ApiError::invalid_request("send title and/or archived"));
        }
        let title = match body.title {
            None => None,
            Some(title) => {
                let title = title.trim().to_owned();
                if title.is_empty() || title.chars().count() > MAX_TITLE_CHARS {
                    return Err(ApiError::invalid_request(format!(
                        "title must be 1-{MAX_TITLE_CHARS} characters"
                    )));
                }
                Some(title)
            }
        };
        self.ensure_exists()?;
        if let Some(title) = title {
            let mut info = self.info()?;
            info.insert("title".to_owned(), json!(title));
            self.store.put_meta("info", &Value::Object(info))?;
            if self
                .store
                .state_at(StateScope::Thread, self.head)?
                .is_some()
            {
                let patch = vec![patch_add("/title", json!(title))];
                self.append_thread_event(
                    StateDelta {
                        scope: StateScope::Thread,
                        patch,
                    },
                    now_ms,
                )?;
            }
        }
        if let Some(archived) = body.archived {
            self.store.put_meta("archived", &json!(archived))?;
        }
        let (title, _) = self.effective_title()?;
        Ok(Reply::new(
            200,
            json!({
                "thread_id": self.thread_id,
                "title": title,
                "archived": self.flag("archived")?,
            }),
        ))
    }

    /// `DELETE /v1/threads/{id}` (admin): a soft delete. The index drops the thread and the
    /// event log stays. 204; `404` for an unknown thread.
    pub fn delete_thread(&mut self) -> Result<Reply, ApiError> {
        self.ensure_exists()?;
        self.store.put_meta("deleted", &json!(true))?;
        Ok(Reply::new(204, Value::Null))
    }

    /// The thread's row in the D1 task index (tasks::index) as of `now_ms`. The Thread DO
    /// upserts it after a change that moves the title, status or lifecycle (`do/thread.rs`).
    pub fn index_row(&self, now_ms: u64) -> Result<IndexRow, ApiError> {
        self.ensure_exists()?;
        let info = self.info()?;
        let (title, title_source) = self.effective_title()?;
        let mut status = STATUS_OPEN;
        for task in self.store.tasks_with_status(TaskStatus::Running)? {
            status = STATUS_RUNNING;
            if !self.store.open_interrupts(&task.task_id)?.is_empty() {
                status = STATUS_WAITING;
                break;
            }
        }
        // `done`: the last task ended in success and nothing is running or queued. It lasts
        // until the next prompt (`finished_ok` is cleared by the next `UserMessage`).
        if status == STATUS_OPEN
            && self.flag("finished_ok")?
            && self
                .store
                .tasks_with_status(TaskStatus::Pending)?
                .is_empty()
        {
            status = STATUS_DONE;
        }
        Ok(IndexRow {
            thread_id: self.thread_id.clone(),
            machine_id: text_of(&info, "machine_id"),
            agent: text_of(&info, "agent"),
            workspace_path: info
                .get("workspace")
                .and_then(|workspace| workspace.get("path"))
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            title,
            title_source,
            status,
            archived: self.flag("archived")?,
            deleted: self.flag("deleted")?,
            updated_at: format_timestamp(now_ms),
            last_seq: i64::try_from(self.head).unwrap_or(i64::MAX),
        })
    }

    /// The stored thread info (`machine_id`, `agent`, `workspace`, `title`, ...), or empty.
    fn info(&self) -> Result<Map<String, Value>, ApiError> {
        Ok(match self.store.get_meta("info")? {
            Some(Value::Object(info)) => info,
            _ => Map::new(),
        })
    }

    /// A boolean lifecycle flag (`archived`, `deleted`); unset is `false`.
    fn flag(&self, key: &str) -> Result<bool, ApiError> {
        Ok(self
            .store
            .get_meta(key)?
            .and_then(|value| value.as_bool())
            .unwrap_or(false))
    }

    /// The title the thread shows, and where it came from: the client's (`user`), else the
    /// auto-title of the first prompt (`derived`), else empty.
    fn effective_title(&self) -> Result<(String, &'static str), ApiError> {
        let info = self.info()?;
        let set = info
            .get("title")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|title| !title.is_empty());
        if let Some(title) = set {
            return Ok((title.to_owned(), "user"));
        }
        let derived = self
            .store
            .get_meta("derived_title")?
            .and_then(|value| value.as_str().map(str::to_owned))
            .unwrap_or_default();
        Ok((derived, "derived"))
    }

    /// `404` for a soft-deleted thread, the same answer as for an unknown one. Every
    /// `/v1/threads/{id}/...` route and the subscription check it (the Worker checks the D1 index
    /// first, `auth::service::authorize_thread`; this is the source of truth).
    pub fn ensure_live(&self) -> Result<(), ApiError> {
        if self.flag("deleted")? {
            return Err(ApiError::not_found(format!("no thread {}", self.thread_id)));
        }
        Ok(())
    }

    /// `404` unless something was recorded for the thread (the same test as `thread_info`).
    fn ensure_exists(&self) -> Result<(), ApiError> {
        if self.head == 0 && self.info()?.is_empty() {
            return Err(ApiError::not_found(format!("no thread {}", self.thread_id)));
        }
        Ok(())
    }

    // ---- idempotency (client-api.md §3) -----------------------------------------------------

    /// Runs `op` once per `(actor, key)`: the same key with the same `request_hash` replays the
    /// stored answer without running `op`; the same key with another hash is
    /// `422 idempotency_mismatch`. Only 2xx answers are stored (a failed attempt may be retried
    /// under the same key), for 24 hours.
    pub fn idempotent<F>(
        &mut self,
        actor: &Actor,
        key: &str,
        request_hash: &str,
        now_ms: u64,
        op: F,
    ) -> Result<Reply, ApiError>
    where
        F: FnOnce(&mut Self) -> Result<Reply, ApiError>,
    {
        if let Some(record) = self.store.get_idempotency(&actor.id, key)?
            && record.created_ms.saturating_add(IDEMPOTENCY_TTL_MS) > now_ms
        {
            if record.request_hash == request_hash {
                return Ok(Reply::new(record.status, record.body));
            }
            return Err(ApiError::new(
                422,
                "idempotency_mismatch",
                format!("Idempotency-Key {key} was used with a different request"),
            ));
        }
        let reply = op(self)?;
        if (200..300).contains(&reply.status) {
            self.store
                .prune_idempotency(now_ms.saturating_sub(IDEMPOTENCY_TTL_MS))?;
            self.store.put_idempotency(&IdempotencyRecord {
                user_id: actor.id.clone(),
                key: key.to_owned(),
                request_hash: request_hash.to_owned(),
                status: reply.status,
                body: reply.body.clone(),
                created_ms: now_ms,
            })?;
        }
        Ok(reply)
    }

    // ---- prompts and the pending queue (client-api.md §5.3) ---------------------------------

    /// `POST /prompts`: starts the task when the thread is idle (appends `UserMessage`, queues a
    /// `prompt` command), otherwise queues it (`StateDelta{thread}` on `/pending_tasks`).
    pub fn submit_prompt(
        &mut self,
        actor: &Actor,
        idempotency_key: &str,
        body: PromptBody,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        if body.parts.is_empty() {
            return Err(ApiError::invalid_request(
                "parts must have at least one part",
            ));
        }
        let task_id = match body.task_id {
            Some(id) => checked_id("task_id", id)?,
            None => self.new_id("tk", now_ms)?,
        };
        let message_id = match body.message_id {
            Some(id) => checked_id("message_id", id)?,
            None => self.new_id("msg", now_ms)?,
        };
        if self.store.get_task(&task_id)?.is_some() {
            return Err(ApiError::invalid_request(format!(
                "task_id {task_id} is already used in this thread"
            )));
        }
        let task = TaskRecord {
            task_id: task_id.clone(),
            status: TaskStatus::Pending,
            message_id,
            preview: preview_of(&body.parts),
            parts: body.parts,
            author: actor.clone(),
            queued_at: format_timestamp(now_ms),
            idempotency_key: idempotency_key.to_owned(),
            has_run: false,
            cancel_requested: false,
        };
        let busy = !self
            .store
            .tasks_with_status(TaskStatus::Running)?
            .is_empty()
            || !self
                .store
                .tasks_with_status(TaskStatus::Pending)?
                .is_empty();
        if !busy {
            self.begin_task(&task, now_ms)?;
            self.store.put_task(&TaskRecord {
                status: TaskStatus::Running,
                ..task.clone()
            })?;
            self.queue_prompt_command(&task, now_ms)?;
            return Ok(Reply::new(
                202,
                json!({ "task_id": task_id, "status": "started" }),
            ));
        }
        self.store.put_task(&task)?;
        self.emit_pending_added(&task, now_ms)?;
        if self
            .store
            .tasks_with_status(TaskStatus::Running)?
            .is_empty()
        {
            // Idle with a queue (a start failed earlier): drain it in order.
            self.start_next_pending(now_ms)?;
        }
        let position = self
            .store
            .tasks_with_status(TaskStatus::Pending)?
            .iter()
            .position(|pending| pending.task_id == task_id);
        Ok(Reply::new(
            202,
            match position {
                Some(index) => {
                    json!({ "task_id": task_id, "status": "pending", "position": index + 1 })
                }
                None => json!({ "task_id": task_id, "status": "started" }),
            },
        ))
    }

    /// `GET /pending`.
    pub fn list_pending(&self) -> Result<Reply, ApiError> {
        let pending: Vec<Value> = self
            .store
            .tasks_with_status(TaskStatus::Pending)?
            .iter()
            .map(|task| {
                json!({
                    "task_id": task.task_id,
                    "preview": task.preview,
                    "author": task.author,
                    "queued_at": task.queued_at,
                })
            })
            .collect();
        Ok(Reply::new(200, json!({ "pending": pending })))
    }

    /// `DELETE /pending/{task_id}`: 204, or `409 task_started` when it already started.
    pub fn remove_pending(&mut self, task_id: &str, now_ms: u64) -> Result<Reply, ApiError> {
        let Some(mut task) = self.store.get_task(task_id)? else {
            return Err(ApiError::not_found(format!("no task {task_id}")));
        };
        if task.status == TaskStatus::Finished && !task.has_run && task.cancel_requested {
            // Already dropped from the queue by an earlier delete or cancel.
            return Err(ApiError::not_found(format!("no pending task {task_id}")));
        }
        if task.status != TaskStatus::Pending {
            return Err(ApiError::new(
                409,
                "task_started",
                format!("task {task_id} already started; cancel it instead"),
            ));
        }
        task.status = TaskStatus::Finished;
        task.cancel_requested = true;
        self.store.put_task(&task)?;
        self.emit_pending_removed(task_id, now_ms)?;
        Ok(Reply::new(204, Value::Null))
    }

    /// Appends `UserMessage` for `task` (the hub emits it when a task starts, client-api.md §6).
    fn begin_task(&mut self, task: &TaskRecord, now_ms: u64) -> Result<(), ApiError> {
        let envelope = Envelope::new(
            self.thread_id.clone(),
            Some(task.task_id.clone()),
            None,
            format_timestamp(now_ms),
            UserMessage {
                message_id: task.message_id.clone(),
                parts: task.parts.clone(),
                author: task.author.clone(),
            },
        );
        self.append_inner(envelope, now_ms).map(drop)
    }

    fn queue_prompt_command(&mut self, task: &TaskRecord, now_ms: u64) -> Result<(), ApiError> {
        let command = Command::Prompt {
            thread_id: self.thread_id.clone(),
            task_id: task.task_id.clone(),
            message_id: task.message_id.clone(),
            parts: task.parts.clone(),
            author: task.author.clone(),
        };
        self.store
            .queue_command(&task.idempotency_key, &format_timestamp(now_ms), &command)?;
        Ok(())
    }

    /// When no task is running, starts the oldest pending one: `UserMessage`, the `prompt`
    /// command and the `StateDelta{thread}` that removes it from `/pending_tasks`.
    fn start_next_pending(&mut self, now_ms: u64) -> Result<(), ApiError> {
        if !self
            .store
            .tasks_with_status(TaskStatus::Running)?
            .is_empty()
        {
            return Ok(());
        }
        while let Some(task) = self
            .store
            .tasks_with_status(TaskStatus::Pending)?
            .into_iter()
            .next()
        {
            match self.begin_task(&task, now_ms) {
                Ok(()) => {
                    self.queue_prompt_command(&task, now_ms)?;
                    return self.emit_pending_removed(&task.task_id, now_ms);
                }
                Err(error) if error.status < 500 => {
                    // This task cannot start (for example a reused task id): drop it.
                    self.store.put_task(&TaskRecord {
                        status: TaskStatus::Finished,
                        ..task.clone()
                    })?;
                    self.emit_pending_removed(&task.task_id, now_ms)?;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(())
    }

    // ---- the `thread` state (title, pending tasks) ------------------------------------------

    fn new_thread_state(&self) -> Result<Map<String, Value>, ApiError> {
        let mut state = Map::new();
        let info = self.store.get_meta("info")?;
        let title = info
            .as_ref()
            .and_then(|info| info.get("title"))
            .cloned()
            .unwrap_or_else(|| Value::String(String::new()));
        state.insert("title".to_owned(), title);
        if let Some(machine_id) = info.as_ref().and_then(|info| info.get("machine_id")) {
            state.insert("machine_id".to_owned(), machine_id.clone());
        }
        state.insert(
            "pending_tasks".to_owned(),
            Value::Array(self.pending_entries()?),
        );
        Ok(state)
    }

    fn pending_entries(&self) -> Result<Vec<Value>, ApiError> {
        Ok(self
            .store
            .tasks_with_status(TaskStatus::Pending)?
            .iter()
            .map(pending_entry)
            .collect())
    }

    fn emit_pending_added(&mut self, task: &TaskRecord, now_ms: u64) -> Result<(), ApiError> {
        match self.store.state_at(StateScope::Thread, self.head)? {
            None => {
                let snapshot = self.new_thread_state()?;
                self.append_thread_event(
                    StateSnapshot {
                        scope: StateScope::Thread,
                        snapshot,
                    },
                    now_ms,
                )
            }
            Some(state) => {
                let patch = if state.get("pending_tasks").is_some_and(Value::is_array) {
                    patch_add("/pending_tasks/-", pending_entry(task))
                } else {
                    patch_add("/pending_tasks", Value::Array(self.pending_entries()?))
                };
                self.append_thread_event(
                    StateDelta {
                        scope: StateScope::Thread,
                        patch: vec![patch],
                    },
                    now_ms,
                )
            }
        }
    }

    fn emit_pending_removed(&mut self, task_id: &str, now_ms: u64) -> Result<(), ApiError> {
        match self.store.state_at(StateScope::Thread, self.head)? {
            None => {
                let snapshot = self.new_thread_state()?;
                self.append_thread_event(
                    StateSnapshot {
                        scope: StateScope::Thread,
                        snapshot,
                    },
                    now_ms,
                )
            }
            Some(state) => {
                let index = state
                    .get("pending_tasks")
                    .and_then(Value::as_array)
                    .and_then(|list| {
                        list.iter().position(|entry| {
                            entry.get("task_id").and_then(Value::as_str) == Some(task_id)
                        })
                    });
                let Some(index) = index else {
                    return Ok(());
                };
                self.append_thread_event(
                    StateDelta {
                        scope: StateScope::Thread,
                        patch: vec![PatchOp {
                            op: PatchOpKind::Remove,
                            path: format!("/pending_tasks/{index}"),
                            from: None,
                            value: None,
                        }],
                    },
                    now_ms,
                )
            }
        }
    }

    fn append_thread_event(
        &mut self,
        event: impl Into<Event>,
        now_ms: u64,
    ) -> Result<(), ApiError> {
        let envelope = Envelope::new(
            self.thread_id.clone(),
            None,
            None,
            format_timestamp(now_ms),
            event,
        );
        self.append_inner(envelope, now_ms).map(drop)
    }

    // ---- interrupts (client-api.md §5.4) ----------------------------------------------------

    /// Accepts the first answer to `interrupt_id` and appends `InterruptResolved` (scope: task).
    /// `404` unknown interrupt, `409 already_resolved` (naming the resolver) when another answer
    /// won, `410 expired` past `expires_at`, `400` for an outcome the interrupt does not allow.
    pub fn resolve_interrupt(
        &mut self,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        resolved_by: Actor,
        now_ms: u64,
    ) -> Result<Resolved, ApiError> {
        let Some(record) = self.store.get_interrupt(interrupt_id)? else {
            return Err(ApiError::not_found(format!("no interrupt {interrupt_id}")));
        };
        if let Some(resolution) = &record.resolved {
            let mut error = ApiError::new(
                409,
                "already_resolved",
                "this interrupt was already answered",
            );
            error.resolved_by = Some(Box::new(resolution.resolved_by.clone()));
            return Err(error);
        }
        if parse_timestamp(&record.expires_at).is_some_and(|expires| now_ms >= expires) {
            return Err(ApiError::new(
                410,
                "expired",
                format!("interrupt {interrupt_id} expired at {}", record.expires_at),
            ));
        }
        check_outcome(&record, &outcome)?;
        let envelope = self.append_resolved(&record, outcome, resolved_by, now_ms)?;
        Ok(Resolved {
            envelope,
            task_id: record.task_id,
        })
    }

    fn append_resolved(
        &mut self,
        record: &InterruptRecord,
        outcome: ResolvedOutcome,
        resolved_by: Actor,
        now_ms: u64,
    ) -> Result<Envelope, ApiError> {
        let envelope = Envelope::new(
            self.thread_id.clone(),
            Some(record.task_id.clone()),
            None,
            format_timestamp(now_ms),
            InterruptResolved {
                interrupt_id: record.interrupt_id.clone(),
                outcome,
                resolved_by,
            },
        );
        self.append_inner(envelope, now_ms)
    }

    /// `POST /interrupts/{id}`: [`ThreadCore::resolve_interrupt`], then the `approval` command
    /// for the runner. 200 `{interrupt_id, resolved_by}`.
    pub fn answer_interrupt(
        &mut self,
        actor: &Actor,
        idempotency_key: &str,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        let resolved =
            self.resolve_interrupt(interrupt_id, outcome.clone(), actor.clone(), now_ms)?;
        // The tunnel `Outcome` is the `InterruptResolved` outcome minus `expired` (SPEC-002a,
        // tunnel.md §3.5). Clients cannot send `expired` (`check_outcome`); it maps to cancelled.
        let tunnel_outcome = match outcome {
            ResolvedOutcome::Selected { option_id } => Outcome::Selected { option_id },
            ResolvedOutcome::Submitted { data } => Outcome::Submitted { data },
            ResolvedOutcome::Declined => Outcome::Declined,
            ResolvedOutcome::Cancelled | ResolvedOutcome::Expired => Outcome::Cancelled,
        };
        let command = Command::Approval {
            thread_id: self.thread_id.clone(),
            task_id: resolved.task_id,
            interrupt_id: interrupt_id.to_owned(),
            outcome: tunnel_outcome,
            resolved_by: actor.clone(),
        };
        self.store
            .queue_command(idempotency_key, &format_timestamp(now_ms), &command)?;
        Ok(Reply::new(
            200,
            json!({ "interrupt_id": interrupt_id, "resolved_by": actor }),
        ))
    }

    /// An interrupt answer that came up the tunnel from the machine's own terminal (tunnel.md
    /// §3.6a): [`ThreadCore::answer_interrupt`] with the machine's local user as the actor, and
    /// `answer_id` as the idempotency key. The verdict is always a 200 body
    /// `{status, resolved_by?, message}` so the Machine DO can pass it on: `accepted`,
    /// `already_resolved` (naming the winner) or `rejected` (unknown, expired, not allowed).
    pub fn answer_from_machine(
        &mut self,
        actor: &Actor,
        answer_id: &str,
        interrupt_id: &str,
        outcome: ResolvedOutcome,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        if !valid_idempotency_key(answer_id) {
            return Err(ApiError::invalid_request(
                "answer_id must be 1-128 characters of [A-Za-z0-9_:.-], not starting with local:",
            ));
        }
        let hash = request_hash(
            "POST",
            interrupt_id,
            &serde_json::to_value(&outcome).unwrap_or(Value::Null),
        );
        self.idempotent(actor, answer_id, &hash, now_ms, |core| {
            let verdict = match core.answer_interrupt(
                actor,
                answer_id,
                interrupt_id,
                outcome.clone(),
                now_ms,
            ) {
                Ok(_) => {
                    json!({ "status": "accepted", "resolved_by": actor, "message": "accepted" })
                }
                Err(error) if error.code == "already_resolved" => json!({
                    "status": "already_resolved",
                    "resolved_by": error.resolved_by,
                    "message": error.message,
                }),
                Err(error) if (400..500).contains(&error.status) => json!({
                    "status": "rejected",
                    "message": format!("{}: {}", error.code, error.message),
                }),
                Err(error) => return Err(error),
            };
            Ok(Reply::new(200, verdict))
        })
    }

    // ---- cancel (client-api.md §5.5) --------------------------------------------------------

    /// `POST /cancel`: records the cancel and abandons the task's open interrupts
    /// (`InterruptResolved{cancelled}`).
    ///
    /// - running task whose runner already got the prompt (or has run): queues a `cancel`
    ///   command; the runner's `RunError{cancelled}` arrives through [`ThreadCore::append`];
    /// - started task whose `prompt` command nobody has picked up yet: the command is
    ///   retracted and the hub itself ends the task (`RunStarted{prompt}` + `RunError{cancelled}`),
    ///   after which the next pending task starts;
    /// - a task still in the pending queue: removed from the queue (it never started, so it has
    ///   no events); 202 `cancelled`;
    /// - a finished task: 200 `already_finished`, no effect.
    pub fn cancel(
        &mut self,
        actor: &Actor,
        idempotency_key: &str,
        task_id: &str,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        let Some(mut task) = self.store.get_task(task_id)? else {
            return Err(ApiError::not_found(format!("no task {task_id}")));
        };
        match task.status {
            TaskStatus::Finished => {
                return Ok(Reply::new(200, json!({ "status": "already_finished" })));
            }
            TaskStatus::Pending => {
                self.remove_pending(task_id, now_ms)?;
                return Ok(Reply::new(
                    202,
                    json!({ "task_id": task_id, "status": "cancelled" }),
                ));
            }
            TaskStatus::Running => {}
        }
        task.cancel_requested = true;
        self.store.put_task(&task)?;
        for record in self.store.open_interrupts(task_id)? {
            self.append_resolved(&record, ResolvedOutcome::Cancelled, actor.clone(), now_ms)?;
        }
        if !task.has_run && self.store.retract_prompt_command(task_id)? {
            self.end_unstarted_task(&task, now_ms)?;
            return Ok(Reply::new(
                202,
                json!({ "task_id": task_id, "status": "cancelled" }),
            ));
        }
        let command = Command::Cancel {
            thread_id: self.thread_id.clone(),
            task_id: task_id.to_owned(),
        };
        self.store
            .queue_command(idempotency_key, &format_timestamp(now_ms), &command)?;
        Ok(Reply::new(
            202,
            json!({ "task_id": task_id, "status": "cancelling" }),
        ))
    }

    // ---- session mode and config (client-api.md §5.5a) --------------------------------------

    /// `POST /session`: checks `mode_id` and `config` against the thread's current `session`
    /// state (`modes`, `config_options`, as the runner's snapshot advertised them), then queues
    /// a `session_config` command (tunnel.md §3.5) for the Machine DO. 202 `{status: "queued"}`;
    /// the runner's change arrives as `StateDelta{session}`. `400` for an empty body or a value
    /// the session did not advertise, `404` for an unknown thread; nothing is queued then.
    pub fn session_config(
        &mut self,
        idempotency_key: &str,
        body: SessionBody,
        now_ms: u64,
    ) -> Result<Reply, ApiError> {
        if body.mode_id.is_none() && body.config.is_none() {
            return Err(ApiError::invalid_request(
                "the session body needs a mode_id, a config, or both",
            ));
        }
        if self.head == 0 && self.store.get_meta("info")?.is_none() {
            return Err(ApiError::not_found(format!("no thread {}", self.thread_id)));
        }
        let state = self
            .store
            .state_at(StateScope::Session, self.head)?
            .unwrap_or_else(|| json!({}));
        if let Some(mode) = &body.mode_id {
            let advertised = state
                .get("modes")
                .and_then(Value::as_array)
                .is_some_and(|modes| {
                    modes
                        .iter()
                        .any(|available| available.get("id").and_then(Value::as_str) == Some(mode))
                });
            if !advertised {
                return Err(ApiError::invalid_request(format!(
                    "mode_id {mode} is not one of the session's modes"
                )));
            }
        }
        let config = match body.config {
            None => None,
            Some(change) => {
                let options = state
                    .get("config_options")
                    .and_then(Value::as_array)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let option = options
                    .iter()
                    .find(|option| option.get("id").and_then(Value::as_str) == Some(&change.id))
                    .ok_or_else(|| {
                        ApiError::invalid_request(format!(
                            "config id {} is not one of the session's config options",
                            change.id
                        ))
                    })?;
                if !config_value_fits(option, &change.value) {
                    return Err(ApiError::invalid_request(format!(
                        "the value does not fit config option {}",
                        change.id
                    )));
                }
                // The command's config type is private to the protocol's command module; the
                // wire shape `{id, value}` is what it deserializes from.
                Some(
                    serde_json::from_value(json!({ "id": change.id, "value": change.value }))
                        .map_err(|error| ApiError::internal(error.to_string()))?,
                )
            }
        };
        let command = Command::SessionConfig {
            thread_id: self.thread_id.clone(),
            mode_id: body.mode_id,
            config,
        };
        self.store
            .queue_command(idempotency_key, &format_timestamp(now_ms), &command)?;
        Ok(Reply::new(
            202,
            json!({ "thread_id": self.thread_id, "status": "queued" }),
        ))
    }

    /// Ends a started task whose runner never saw it: a run that starts and is cancelled at
    /// once (the §5 validator only accepts `RunError` inside an open run).
    fn end_unstarted_task(&mut self, task: &TaskRecord, now_ms: u64) -> Result<(), ApiError> {
        let run_id = self.new_id("run", now_ms)?;
        let machine_id = self.machine_id()?;
        let ts = format_timestamp(now_ms);
        self.append_inner(
            Envelope::new(
                self.thread_id.clone(),
                Some(task.task_id.clone()),
                Some(run_id.clone()),
                ts.clone(),
                RunStarted {
                    reason: RunReason::Prompt,
                    parent_run_id: None,
                    agent_id: None,
                    machine_id,
                },
            ),
            now_ms,
        )?;
        self.append_inner(
            Envelope::new(
                self.thread_id.clone(),
                Some(task.task_id.clone()),
                Some(run_id),
                ts,
                RunError {
                    reason: RunErrorReason::Cancelled,
                    code: Some("cancelled_before_start".to_owned()),
                    message: "cancelled before the runner started the task".to_owned(),
                },
            ),
            now_ms,
        )
        .map(drop)
    }

    // ---- commands outbox (the hook for the Machine DO, M2B-002) -----------------------------

    /// Commands waiting for the runner, oldest first, as
    /// `{id, idempotency_key, issued_at, machine_id, command}`. The Machine DO of
    /// `machine_id` sends each as a tunnel `command` frame and calls
    /// [`ThreadCore::ack_commands`] once the runner has acknowledged it.
    pub fn pending_commands(&self, limit: usize) -> Result<Vec<Value>, ApiError> {
        let machine_id = self.machine_id()?;
        Ok(self
            .store
            .unacked_commands(limit)?
            .into_iter()
            .map(
                |CommandRecord {
                     id,
                     idempotency_key,
                     issued_at,
                     command,
                 }| {
                    json!({
                        "id": id,
                        "idempotency_key": idempotency_key,
                        "issued_at": issued_at,
                        "machine_id": machine_id,
                        "command": command,
                    })
                },
            )
            .collect())
    }

    /// Marks commands delivered (see [`ThreadCore::pending_commands`]); returns how many changed.
    pub fn ack_commands(&mut self, ids: &[u64]) -> Result<usize, ApiError> {
        Ok(self.store.ack_commands(ids)?)
    }

    // ---- ids --------------------------------------------------------------------------------

    fn new_id(&mut self, prefix: &str, now_ms: u64) -> Result<String, ApiError> {
        let n = self.store.next_counter()?;
        Ok(format!("{prefix}_{now_ms:x}_{n:x}"))
    }
}

/// The `{"type":"event","envelope":…}` subscription message (turn-event.md §6).
pub fn event_message(envelope: &Envelope) -> Value {
    json!({ "type": "event", "envelope": envelope })
}

/// Stable hash of a request, for `Idempotency-Key` comparison (two 64-bit FNV-1a lanes; not a
/// security boundary: a key is scoped to one user).
pub fn request_hash(method: &str, path: &str, body: &Value) -> String {
    let canonical = format!("{method} {path}\n{body}");
    let lane = |offset: u64| {
        canonical.bytes().fold(offset, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0000_0100_0000_01b3)
        })
    };
    format!(
        "{:016x}{:016x}",
        lane(0xcbf2_9ce4_8422_2325),
        lane(0x8422_2325_cbf2_9ce4)
    )
}

/// `true` when an `Idempotency-Key` header value is valid (client-api.md §3).
///
/// A key starting with `local:` is refused. Older runners read that prefix as "a prompt typed
/// locally" (client-api.md §6), so a client must not be able to send one.
pub fn valid_idempotency_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && !key.starts_with("local:")
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
}

/// `true` for the events that end a task: `RunFinished{success}` and `RunError` (§5 rule 1).
fn ends_task(event: &Event) -> bool {
    match event {
        Event::RunFinished(finished) => matches!(finished.outcome, RunOutcome::Success { .. }),
        Event::RunError(_) => true,
        _ => false,
    }
}

fn checked_id(field: &str, id: String) -> Result<String, ApiError> {
    if is_valid_id(&id) {
        Ok(id)
    } else {
        Err(ApiError::invalid_request(format!(
            "{field} must be 1-128 characters of [A-Za-z0-9_:.-]"
        )))
    }
}

/// A string field of the thread info, or empty.
fn text_of(info: &Map<String, Value>, key: &str) -> String {
    info.get(key)
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_owned()
}

fn preview_of(parts: &[ContentPart]) -> String {
    parts
        .first()
        .map(|part| match part {
            ContentPart::Text { text } => text.chars().take(PREVIEW_CHARS).collect(),
            ContentPart::File { name, .. } => name.chars().take(PREVIEW_CHARS).collect(),
        })
        .unwrap_or_default()
}

fn pending_entry(task: &TaskRecord) -> Value {
    json!({ "task_id": task.task_id, "preview": task.preview, "author": task.author })
}

fn patch_add(path: &str, value: Value) -> PatchOp {
    PatchOp {
        op: PatchOpKind::Add,
        path: path.to_owned(),
        from: None,
        value: Some(value),
    }
}

/// `true` when `value` fits the advertised config `option`: a boolean for a boolean option, a
/// listed value id for a select option (flat or grouped `options`).
fn config_value_fits(option: &Value, value: &Value) -> bool {
    match option.get("type").and_then(Value::as_str) {
        Some("boolean") => value.is_boolean(),
        Some("select") => value
            .as_str()
            .is_some_and(|text| select_values(option.get("options")).contains(&text)),
        _ => false,
    }
}

/// Every value id a select option lists, flat or inside groups.
fn select_values(options: Option<&Value>) -> Vec<&str> {
    options
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .flat_map(|entry| match entry.get("options") {
            Some(group) => select_values(Some(group)),
            None => entry
                .get("value")
                .and_then(Value::as_str)
                .into_iter()
                .collect(),
        })
        .collect()
}

/// Which outcomes an interrupt accepts from a client.
fn check_outcome(record: &InterruptRecord, outcome: &ResolvedOutcome) -> Result<(), ApiError> {
    let bad = |message: String| Err(ApiError::invalid_request(message));
    match outcome {
        ResolvedOutcome::Selected { option_id } => {
            if record.kind != InterruptKind::Approval {
                return bad("selected answers an approval".to_owned());
            }
            if !record.option_ids.is_empty() && !record.option_ids.contains(option_id) {
                return bad(format!("{option_id} is not one of the options"));
            }
            Ok(())
        }
        ResolvedOutcome::Submitted { .. } if record.kind != InterruptKind::Form => {
            bad("submitted answers a form".to_owned())
        }
        ResolvedOutcome::Declined if record.kind == InterruptKind::Approval => {
            bad("declined answers a form or an auth request".to_owned())
        }
        ResolvedOutcome::Expired => bad("expired is set by the system, not by a client".to_owned()),
        _ => Ok(()),
    }
}
