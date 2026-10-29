//! The outbound queue: prompts, approvals and cancels that this client wants the hub to
//! accept (client-api.md §3, plan §4.4).
//!
//! - Every intent gets an `Idempotency-Key` once, when it is enqueued; the key is stored with
//!   the intent, so a retry after a timeout, a restart or a reload sends the same key and the
//!   hub replays its answer instead of acting twice.
//! - Transient failures (transport errors, 408/425/429, 5xx) are retried with exponential
//!   backoff and full jitter. Clock and randomness are injected, so the schedule is testable.
//! - `409 already_resolved` is terminal and surfaces the resolver ([`Outbox::conflicts`]).
//! - `409 task_started` on a [`Intent::RemovePending`] is terminal too: the task left the queue
//!   before the removal arrived, so the caller should cancel instead
//!   ([`Outbox::started_removals`]).
//! - Other 4xx responses (and 410 `expired`) are terminal and surface as `Rejected`.

use std::collections::{BTreeMap, HashSet};
use std::future::Future;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use protocol::tunnel::{Actor, ContentPart, Outcome};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::cache::{Cache, CacheError};
use crate::connectivity::Connectivity;

// ---- injected time and randomness ----------------------------------------------------------

/// Wall-clock time in milliseconds.
pub trait Clock: Send + Sync {
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .map(|elapsed| u64::try_from(elapsed.as_millis()).unwrap_or(u64::MAX))
            .unwrap_or(0)
    }
}

/// A clock that only moves when told to. For tests and the scheduling simulations.
#[derive(Debug, Default)]
pub struct ManualClock {
    now: AtomicU64,
}

impl ManualClock {
    pub fn new(start_ms: u64) -> Self {
        Self {
            now: AtomicU64::new(start_ms),
        }
    }

    pub fn set(&self, now_ms: u64) {
        self.now.store(now_ms, Ordering::SeqCst);
    }

    pub fn advance(&self, by_ms: u64) {
        self.now.fetch_add(by_ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.now.load(Ordering::SeqCst)
    }
}

/// A source of uniform integers.
pub trait Random: Send {
    /// A value in `0..bound`. `bound` is at least 1.
    fn below(&mut self, bound: u64) -> u64;
}

/// The thread-local system generator.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemRandom;

impl Random for SystemRandom {
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 {
            return 0;
        }
        rand::random_range(0..bound)
    }
}

/// Always returns `value`, capped to the bound. Pins the jitter in tests: `u64::MAX` gives the
/// top of the window, `0` the bottom.
#[derive(Debug, Clone, Copy)]
pub struct FixedRandom(pub u64);

impl Random for FixedRandom {
    fn below(&mut self, bound: u64) -> u64 {
        if bound == 0 { 0 } else { self.0.min(bound - 1) }
    }
}

// ---- intents -----------------------------------------------------------------------------

/// A write the user asked for.
///
/// `machine_id`, `author` and `resolved_by` are set only by the MVP-000 hub, whose
/// routes require them in the body. The production hub derives the actor from the token
/// (client-api.md §1), so clients leave these `None` and they are not sent.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Intent {
    /// `POST /threads/{thread}/prompts`.
    Prompt {
        thread_id: String,
        task_id: String,
        message_id: String,
        parts: Vec<ContentPart>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        machine_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        author: Option<Actor>,
    },
    /// `POST /threads/{thread}/interrupts/{interrupt}`.
    Approval {
        thread_id: String,
        interrupt_id: String,
        outcome: Outcome,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        resolved_by: Option<Actor>,
    },
    /// `POST /threads/{thread}/cancel`.
    Cancel { thread_id: String, task_id: String },
    /// `POST /threads/{thread}/session` (client-api.md §5.5a): switch the session mode, change
    /// one config option, or both. At least one of `mode_id` and `config` is set.
    SetSession {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        mode_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config: Option<SessionConfig>,
    },
    /// `DELETE /threads/{thread}/pending/{task}` (client-api.md §5.3): drop a queued prompt.
    /// `409 task_started` means it already started; see [`Outbox::started_removals`].
    RemovePending { thread_id: String, task_id: String },
}

/// One session config option change: `{ "id": "model", "value": "opus" }`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionConfig {
    pub id: String,
    pub value: String,
}

/// The HTTP method of an outbox request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Method {
    Post,
    Delete,
}

impl Method {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Post => "POST",
            Self::Delete => "DELETE",
        }
    }
}

impl Intent {
    pub fn thread_id(&self) -> &str {
        match self {
            Self::Prompt { thread_id, .. }
            | Self::Approval { thread_id, .. }
            | Self::Cancel { thread_id, .. }
            | Self::SetSession { thread_id, .. }
            | Self::RemovePending { thread_id, .. } => thread_id,
        }
    }

    /// The HTTP method.
    pub fn method(&self) -> Method {
        match self {
            Self::RemovePending { .. } => Method::Delete,
            _ => Method::Post,
        }
    }

    /// The path under the hub's base URL.
    pub fn path(&self) -> String {
        match self {
            Self::Prompt { thread_id, .. } => format!("/threads/{thread_id}/prompts"),
            Self::Approval {
                thread_id,
                interrupt_id,
                ..
            } => format!("/threads/{thread_id}/interrupts/{interrupt_id}"),
            Self::Cancel { thread_id, .. } => format!("/threads/{thread_id}/cancel"),
            Self::SetSession { thread_id, .. } => format!("/threads/{thread_id}/session"),
            Self::RemovePending { thread_id, task_id } => {
                format!("/threads/{thread_id}/pending/{task_id}")
            }
        }
    }

    /// The JSON body (client-api.md §5.3 to §5.5a, plus the MVP-000 fields when set).
    /// `null` for a `DELETE`, which has none.
    pub fn body(&self) -> Value {
        match self {
            Self::Prompt {
                task_id,
                message_id,
                parts,
                machine_id,
                author,
                ..
            } => {
                let mut body =
                    json!({ "task_id": task_id, "message_id": message_id, "parts": parts });
                if let Some(object) = body.as_object_mut() {
                    if let Some(machine_id) = machine_id {
                        object.insert("machine_id".to_owned(), json!(machine_id));
                    }
                    if let Some(author) = author {
                        object.insert("author".to_owned(), json!(author));
                    }
                }
                body
            }
            Self::Approval {
                outcome,
                resolved_by,
                ..
            } => {
                let mut body = json!({ "outcome": outcome });
                if let (Some(object), Some(actor)) = (body.as_object_mut(), resolved_by) {
                    object.insert("resolved_by".to_owned(), json!(actor));
                }
                body
            }
            Self::Cancel { task_id, .. } => json!({ "task_id": task_id }),
            Self::SetSession {
                mode_id, config, ..
            } => {
                let mut body = json!({});
                if let Some(object) = body.as_object_mut() {
                    if let Some(mode_id) = mode_id {
                        object.insert("mode_id".to_owned(), json!(mode_id));
                    }
                    if let Some(config) = config {
                        object.insert("config".to_owned(), json!(config));
                    }
                }
                body
            }
            Self::RemovePending { .. } => Value::Null,
        }
    }
}

/// Where an intent stands.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum IntentState {
    /// Waiting for its first attempt, or for its next retry.
    Queued,
    /// Accepted (2xx). `body` is the hub's answer.
    Done { status: u16, body: Value },
    /// `409 already_resolved`: another device answered first. `resolved_by` is who.
    Conflict {
        status: u16,
        resolved_by: Option<Actor>,
        /// The hub's `error` code (`already_resolved`, `task_started`), when it sent one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
    /// Refused for good (other 4xx, including `410 expired`).
    Rejected { status: u16, error: String },
    /// Transient failures used up the retry budget.
    Exhausted { last_error: String },
}

impl IntentState {
    /// `true` for every state except `Queued`.
    pub fn is_terminal(&self) -> bool {
        !matches!(self, Self::Queued)
    }
}

/// One outbox entry, as stored.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct QueuedIntent {
    /// The row id in the cache. Set on load and on enqueue; not stored in the record itself.
    #[serde(skip)]
    pub local_id: i64,
    /// `idk_` plus random hex, generated once per intent (client-api.md §3).
    pub idempotency_key: String,
    pub intent: Intent,
    pub state: IntentState,
    /// Failed attempts so far.
    pub failures: u32,
    /// Earliest time of the next attempt (ms), while `Queued`.
    pub next_attempt_at_ms: u64,
    pub last_error: Option<String>,
    pub last_status: Option<u16>,
    pub created_at_ms: u64,
}

/// A `409 task_started` for a pending-task removal this client sent: the task had already
/// started, so removing it is too late (cancel it instead).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StartedRemoval {
    pub local_id: i64,
    pub thread_id: String,
    pub task_id: String,
}

/// A `409` for an approval this client sent: the first answer won.
#[derive(Debug, Clone, PartialEq)]
pub struct Conflict {
    pub local_id: i64,
    pub thread_id: String,
    pub interrupt_id: String,
    pub resolved_by: Option<Actor>,
}

// ---- retry policy --------------------------------------------------------------------------

/// Backoff parameters.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct RetryPolicy {
    /// The first retry's window, in ms.
    pub base_ms: u64,
    /// The largest window, in ms.
    pub cap_ms: u64,
    /// Failed attempts before an intent is `Exhausted`.
    pub max_failures: u32,
}

impl Default for RetryPolicy {
    fn default() -> Self {
        Self {
            base_ms: 500,
            cap_ms: 60_000,
            max_failures: 12,
        }
    }
}

impl RetryPolicy {
    /// Delay before the attempt that follows `failures` failed attempts (`failures >= 1`).
    /// Full jitter: uniform in `0..=min(cap, base * 2^(failures - 1))`.
    pub fn delay_ms(&self, failures: u32, rng: &mut dyn Random) -> u64 {
        let exponent = failures.saturating_sub(1).min(32);
        let window = self
            .base_ms
            .saturating_mul(1u64 << exponent)
            .min(self.cap_ms);
        rng.below(window.saturating_add(1))
    }
}

// ---- transport -----------------------------------------------------------------------------

/// One request to send (a POST, or the DELETE of a pending task).
#[derive(Debug, Clone, PartialEq)]
pub struct PostRequest {
    pub method: Method,
    pub url: String,
    pub idempotency_key: String,
    /// `null` for a DELETE: no body is sent.
    pub body: Value,
}

/// An HTTP answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PostResponse {
    pub status: u16,
    pub body: Value,
}

/// The request did not get an answer (connection refused, timeout, reset).
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
#[error("transport: {0}")]
pub struct TransportError(pub String);

/// Sends one POST. Implemented by [`HttpTransport`]; tests implement it with scripts.
pub trait Transport {
    fn post(
        &self,
        request: PostRequest,
    ) -> impl Future<Output = Result<PostResponse, TransportError>> + Send;
}

/// `reqwest` transport. Sends the bearer token when one is set.
#[derive(Debug, Clone, Default)]
pub struct HttpTransport {
    client: reqwest::Client,
    token: Option<String>,
}

impl HttpTransport {
    pub fn new(token: Option<String>) -> Self {
        Self {
            client: reqwest::Client::new(),
            token,
        }
    }
}

impl Transport for HttpTransport {
    async fn post(&self, request: PostRequest) -> Result<PostResponse, TransportError> {
        let mut builder = match request.method {
            Method::Post => self.client.post(&request.url).json(&request.body),
            Method::Delete => self.client.delete(&request.url),
        }
        .header("Idempotency-Key", &request.idempotency_key);
        if let Some(token) = &self.token {
            builder = builder.bearer_auth(token);
        }
        let response = builder
            .send()
            .await
            .map_err(|error| TransportError(error.to_string()))?;
        let status = response.status().as_u16();
        let text = response
            .text()
            .await
            .map_err(|error| TransportError(error.to_string()))?;
        let body = serde_json::from_str(&text).unwrap_or(Value::Null);
        Ok(PostResponse { status, body })
    }
}

// ---- the queue -----------------------------------------------------------------------------

/// Outbox failures (persistence or a bad key). Network failures are entry states, not errors.
#[derive(Debug, thiserror::Error)]
pub enum OutboxError {
    #[error(transparent)]
    Cache(#[from] CacheError),
    #[error("outbox record: {0}")]
    Json(#[from] serde_json::Error),
    #[error("idempotency key {0:?} is not 1-128 characters of [A-Za-z0-9_:.-]")]
    BadKey(String),
    #[error("no outbox entry {0}")]
    Unknown(i64),
}

/// What one [`Outbox::flush`] did.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct FlushReport {
    pub sent: usize,
    pub done: usize,
    pub conflicts: usize,
    pub rejected: usize,
    /// Failed attempts that were rescheduled.
    pub retrying: usize,
    /// Transport failures (the hub did not answer): the connectivity signal.
    pub transport_failures: usize,
}

/// A persisted queue of intents.
#[derive(Debug, Clone)]
pub struct Outbox {
    entries: BTreeMap<i64, QueuedIntent>,
    policy: RetryPolicy,
    connectivity: Connectivity,
}

/// A fresh idempotency key: `idk_` plus 32 random hex digits.
pub fn new_idempotency_key() -> String {
    format!("idk_{:032x}", rand::random::<u128>())
}

pub(crate) fn valid_key(key: &str) -> bool {
    (1..=128).contains(&key.len())
        && key
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b':' | b'.' | b'-'))
}

impl Outbox {
    /// Loads every stored entry. Keys and retry state come back exactly as they were saved.
    pub fn load(cache: &Cache, policy: RetryPolicy) -> Result<Self, OutboxError> {
        let mut entries = BTreeMap::new();
        for (id, record) in cache.outbox_rows()? {
            let mut entry: QueuedIntent = serde_json::from_str(&record)?;
            entry.local_id = id;
            entries.insert(id, entry);
        }
        Ok(Self {
            entries,
            policy,
            connectivity: Connectivity::default(),
        })
    }

    pub fn policy(&self) -> RetryPolicy {
        self.policy
    }

    /// The hub link as the outbox last saw it. A transport failure makes it `Offline`; any
    /// answer from the hub (whatever the status) makes it `Online` again. `Connecting` until
    /// the first POST gets a result; [`crate::Connection`] ignores that
    /// initial `Connecting` while nothing is queued.
    pub fn connectivity(&self) -> Connectivity {
        self.connectivity
    }

    /// Queues `intent` with a new idempotency key. Returns its local id.
    pub fn enqueue(
        &mut self,
        cache: &Cache,
        intent: Intent,
        now_ms: u64,
    ) -> Result<i64, OutboxError> {
        self.enqueue_with_key(cache, new_idempotency_key(), intent, now_ms)
    }

    /// Queues `intent` under `key`. Used when the caller already generated the key.
    pub fn enqueue_with_key(
        &mut self,
        cache: &Cache,
        key: String,
        intent: Intent,
        now_ms: u64,
    ) -> Result<i64, OutboxError> {
        if !valid_key(&key) {
            return Err(OutboxError::BadKey(key));
        }
        let entry = QueuedIntent {
            local_id: 0,
            idempotency_key: key,
            intent,
            state: IntentState::Queued,
            failures: 0,
            next_attempt_at_ms: now_ms,
            last_error: None,
            last_status: None,
            created_at_ms: now_ms,
        };
        let record = serde_json::to_string(&entry)?;
        let id = cache.insert_outbox(&record)?;
        let mut entry = entry;
        entry.local_id = id;
        self.entries.insert(id, entry);
        Ok(id)
    }

    pub fn get(&self, local_id: i64) -> Option<&QueuedIntent> {
        self.entries.get(&local_id)
    }

    /// Every entry, in enqueue order.
    pub fn entries(&self) -> impl Iterator<Item = &QueuedIntent> {
        self.entries.values()
    }

    /// The lowest-id queued entry that is due at `now_ms`.
    pub fn next_due(&self, now_ms: u64) -> Option<i64> {
        self.next_due_excluding(now_ms, &HashSet::new())
    }

    /// The earliest time at which a queued entry is due.
    pub fn next_attempt_at(&self) -> Option<u64> {
        self.entries
            .values()
            .filter(|entry| matches!(entry.state, IntentState::Queued))
            .map(|entry| entry.next_attempt_at_ms)
            .min()
    }

    /// Every approval that lost a race to another device.
    pub fn conflicts(&self) -> Vec<Conflict> {
        self.entries
            .values()
            .filter_map(|entry| match (&entry.intent, &entry.state) {
                (
                    Intent::Approval {
                        thread_id,
                        interrupt_id,
                        ..
                    },
                    IntentState::Conflict { resolved_by, .. },
                ) => Some(Conflict {
                    local_id: entry.local_id,
                    thread_id: thread_id.clone(),
                    interrupt_id: interrupt_id.clone(),
                    resolved_by: resolved_by.clone(),
                }),
                _ => None,
            })
            .collect()
    }

    /// Every pending-task removal that came too late (`409 task_started`).
    pub fn started_removals(&self) -> Vec<StartedRemoval> {
        self.entries
            .values()
            .filter_map(|entry| match (&entry.intent, &entry.state) {
                (Intent::RemovePending { thread_id, task_id }, IntentState::Conflict { .. }) => {
                    Some(StartedRemoval {
                        local_id: entry.local_id,
                        thread_id: thread_id.clone(),
                        task_id: task_id.clone(),
                    })
                }
                _ => None,
            })
            .collect()
    }

    /// Removes an entry (for example a rejected one the user dismissed, or a finished one).
    pub fn dismiss(&mut self, cache: &Cache, local_id: i64) -> Result<(), OutboxError> {
        if self.entries.remove(&local_id).is_none() {
            return Err(OutboxError::Unknown(local_id));
        }
        cache.delete_outbox(local_id)?;
        Ok(())
    }

    /// Records a response for `local_id`: the state moves on, and a retry is scheduled when the
    /// failure is transient.
    pub fn record_response(
        &mut self,
        cache: &Cache,
        local_id: i64,
        now_ms: u64,
        response: PostResponse,
        rng: &mut dyn Random,
    ) -> Result<IntentState, OutboxError> {
        // Any answer proves the hub is reachable, so the link is up whatever the status.
        self.connectivity = Connectivity::Online;
        let status = response.status;
        let state = match status {
            200..=299 => IntentState::Done {
                status,
                body: response.body,
            },
            409 => IntentState::Conflict {
                status,
                resolved_by: response
                    .body
                    .get("resolved_by")
                    .and_then(|value| serde_json::from_value::<Actor>(value.clone()).ok()),
                error: response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            },
            408 | 425 | 429 | 500..=599 => {
                return self.schedule_retry(
                    cache,
                    local_id,
                    now_ms,
                    Some(status),
                    format!("HTTP {status}"),
                    rng,
                );
            }
            _ => IntentState::Rejected {
                status,
                error: response
                    .body
                    .get("error")
                    .and_then(Value::as_str)
                    .map(str::to_owned)
                    .unwrap_or_else(|| format!("HTTP {status}")),
            },
        };
        self.finish(cache, local_id, Some(status), state)
    }

    /// Records that no response arrived. Schedules a retry (or gives up).
    pub fn record_transport_failure(
        &mut self,
        cache: &Cache,
        local_id: i64,
        now_ms: u64,
        error: &TransportError,
        rng: &mut dyn Random,
    ) -> Result<IntentState, OutboxError> {
        self.connectivity = Connectivity::Offline;
        self.schedule_retry(cache, local_id, now_ms, None, error.0.clone(), rng)
    }

    /// Sends every entry that is due at the clock's time, in id order, once per call. Each
    /// entry is sent with its own stored key.
    pub async fn flush<T: Transport>(
        &mut self,
        cache: &Cache,
        transport: &T,
        base_url: &str,
        clock: &dyn Clock,
        rng: &mut dyn Random,
    ) -> Result<FlushReport, OutboxError> {
        let base = base_url.trim_end_matches('/');
        let mut report = FlushReport::default();
        let mut attempted = HashSet::new();
        loop {
            let now = clock.now_ms();
            let Some(local_id) = self.next_due_excluding(now, &attempted) else {
                break;
            };
            attempted.insert(local_id);
            let Some(entry) = self.entries.get(&local_id) else {
                continue;
            };
            let request = PostRequest {
                method: entry.intent.method(),
                url: format!("{base}{}", entry.intent.path()),
                idempotency_key: entry.idempotency_key.clone(),
                body: entry.intent.body(),
            };
            report.sent += 1;
            match transport.post(request).await {
                Ok(response) => {
                    let state = self.record_response(cache, local_id, now, response, rng)?;
                    match state {
                        IntentState::Done { .. } => report.done += 1,
                        IntentState::Conflict { .. } => report.conflicts += 1,
                        IntentState::Rejected { .. } | IntentState::Exhausted { .. } => {
                            report.rejected += 1;
                        }
                        IntentState::Queued => report.retrying += 1,
                    }
                }
                Err(error) => {
                    report.transport_failures += 1;
                    self.record_transport_failure(cache, local_id, now, &error, rng)?;
                    report.retrying += 1;
                }
            }
        }
        Ok(report)
    }

    fn next_due_excluding(&self, now_ms: u64, skip: &HashSet<i64>) -> Option<i64> {
        self.entries
            .values()
            .find(|entry| {
                matches!(entry.state, IntentState::Queued)
                    && entry.next_attempt_at_ms <= now_ms
                    && !skip.contains(&entry.local_id)
            })
            .map(|entry| entry.local_id)
    }

    fn schedule_retry(
        &mut self,
        cache: &Cache,
        local_id: i64,
        now_ms: u64,
        status: Option<u16>,
        error: String,
        rng: &mut dyn Random,
    ) -> Result<IntentState, OutboxError> {
        let entry = self
            .entries
            .get_mut(&local_id)
            .ok_or(OutboxError::Unknown(local_id))?;
        entry.failures += 1;
        entry.last_error = Some(error.clone());
        entry.last_status = status;
        let state = if entry.failures >= self.policy.max_failures {
            IntentState::Exhausted { last_error: error }
        } else {
            entry.next_attempt_at_ms =
                now_ms.saturating_add(self.policy.delay_ms(entry.failures, rng));
            IntentState::Queued
        };
        entry.state = state.clone();
        self.save(cache, local_id)?;
        Ok(state)
    }

    fn finish(
        &mut self,
        cache: &Cache,
        local_id: i64,
        status: Option<u16>,
        state: IntentState,
    ) -> Result<IntentState, OutboxError> {
        let entry = self
            .entries
            .get_mut(&local_id)
            .ok_or(OutboxError::Unknown(local_id))?;
        entry.last_status = status;
        entry.last_error = match &state {
            IntentState::Rejected { error, .. } => Some(error.clone()),
            _ => None,
        };
        entry.state = state.clone();
        self.save(cache, local_id)?;
        Ok(state)
    }

    fn save(&self, cache: &Cache, local_id: i64) -> Result<(), OutboxError> {
        let entry = self
            .entries
            .get(&local_id)
            .ok_or(OutboxError::Unknown(local_id))?;
        cache.update_outbox(local_id, &serde_json::to_string(entry)?)?;
        Ok(())
    }
}
