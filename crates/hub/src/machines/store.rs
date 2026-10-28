//! Storage behind the Machine DO: the [`MachineStore`] trait, the persisted [`MachineState`]
//! and an in-memory implementation for native tests. The SQLite one (`sqlite.rs`, wasm32 only)
//! keeps the same rows in the DO's own database.

use protocol::tunnel::{Command, DedupeWindow, Disposition};
use serde::{Deserialize, Serialize};

pub use crate::threads::{StoreError, StoreResult};

/// Where a hosted thread lives: the Thread DO.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum HostKind {
    #[default]
    Thread,
}

impl HostKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Thread => "thread",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "thread" => Some(Self::Thread),
            _ => None,
        }
    }
}

/// Everything the Machine DO persists in one row (JSON). The outbox, the hosted threads, the
/// event-id window and the forwarded-command records are separate tables.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct MachineState {
    /// Empty until the machine is provisioned.
    pub machine_id: String,
    pub org_id: String,
    /// SHA-256 (hex) of the credential; `None` once revoked.
    pub credential_hash: Option<String>,
    pub revoked: bool,

    // ---- up stream (runner -> hub), tunnel.md §4.1 -----------------------------------------
    /// Highest up `seq` durably processed on this stream.
    pub up_last: u64,
    /// Highest up `seq` the hub has acked on the wire.
    pub up_acked: u64,
    /// Increments on every fresh stream; guards late completions of an older stream.
    pub stream_epoch: u64,

    // ---- down stream (hub -> runner) -------------------------------------------------------
    /// Highest down `seq` the runner has acked (the outbox front is `down_base + 1`).
    pub down_base: u64,
    /// Highest down `seq` put on the wire (never below `down_base`).
    pub down_sent: u64,

    // ---- resume token (only the hash is stored), tunnel.md §4.3 -----------------------------
    pub resume_hash: Option<String>,
    pub resume_expires_ms: u64,

    // ---- connection ------------------------------------------------------------------------
    pub conn_counter: u64,
    /// The accepted socket's number; 0 when there is none.
    pub cur_conn: u64,
    /// `hello` was answered with `welcome` on `cur_conn`.
    pub hello_done: bool,
    pub heartbeat_ms: u64,
    /// Last time any frame arrived (ms since the epoch).
    pub last_seen_ms: u64,
    pub runner_version: String,
    pub capabilities: Option<serde_json::Value>,

    // ---- presence --------------------------------------------------------------------------
    /// `online`, `asleep` or `offline`.
    pub presence: String,
    pub presence_since: String,
    /// The runner said it is going to sleep (`presence{asleep}` or `close{going_to_sleep}`).
    pub asleep_hint: bool,
}

/// A command waiting in the durable outbox. The wire `seq` is not stored: it is
/// `down_base + position + 1`, so removing a never-sent row renumbers the rest (tunnel.md §5).
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRow {
    /// Storage order key (never on the wire).
    pub id: u64,
    pub thread_id: String,
    /// The thread's command id.
    pub source_id: String,
    pub host: HostKind,
    pub idempotency_key: String,
    pub issued_at: String,
    pub enqueued_ms: u64,
    pub command: Command,
}

/// What [`MachineStore::outbox_push`] takes (the store assigns `id`).
#[derive(Debug, Clone, PartialEq)]
pub struct NewOutbox {
    pub thread_id: String,
    pub source_id: String,
    pub host: HostKind,
    pub idempotency_key: String,
    pub issued_at: String,
    pub enqueued_ms: u64,
    pub command: Command,
}

/// Life of a command the Thread DO pushed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ForwardState {
    /// In the outbox.
    Queued,
    /// The runner acked the frame (the Thread DO may not know yet).
    Delivered,
    /// The Thread DO withdrew it before it went on the wire.
    Retracted,
    /// Dropped after 7 days without delivery (tunnel.md §5).
    Expired,
}

impl ForwardState {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Queued => "queued",
            Self::Delivered => "delivered",
            Self::Retracted => "retracted",
            Self::Expired => "expired",
        }
    }

    pub fn parse(text: &str) -> Option<Self> {
        match text {
            "queued" => Some(Self::Queued),
            "delivered" => Some(Self::Delivered),
            "retracted" => Some(Self::Retracted),
            "expired" => Some(Self::Expired),
            _ => None,
        }
    }
}

/// A pushed command, remembered so a re-push of the same source is not queued twice, and so a
/// `command_result` (which names only the idempotency key) finds its task.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ForwardedRecord {
    pub thread_id: String,
    pub source_id: String,
    pub host: HostKind,
    pub idempotency_key: String,
    /// `prompt`, `approval`, `dispatch` or `cancel`.
    pub kind: String,
    pub task_id: String,
    pub state: ForwardState,
    pub updated_ms: u64,
}

/// The Machine DO's storage. Every method is synchronous (DO SQLite is).
pub trait MachineStore {
    fn load(&self) -> StoreResult<Option<MachineState>>;
    fn save(&mut self, state: &MachineState) -> StoreResult<()>;

    /// The outbox, oldest first.
    fn outbox(&self) -> StoreResult<Vec<OutboxRow>>;
    fn outbox_push(&mut self, row: NewOutbox) -> StoreResult<()>;
    fn outbox_remove(&mut self, ids: &[u64]) -> StoreResult<()>;

    /// Registers a hosted thread (idempotent; a later registration updates the host).
    fn host_thread(&mut self, thread_id: &str, host: HostKind, now_ms: u64) -> StoreResult<()>;
    fn hosted(&self, thread_id: &str) -> StoreResult<Option<HostKind>>;
    fn hosted_threads(&self) -> StoreResult<Vec<(String, HostKind)>>;

    /// `true` when `event_id` was recorded within the last 24 h (tunnel.md §4.4).
    fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool>;
    /// Records `event_id`. Called only after the effect of its frame is durable.
    fn record_event(&mut self, event_id: &str, now_ms: u64) -> StoreResult<()>;

    fn forwarded(&self, thread_id: &str, source_id: &str) -> StoreResult<Option<ForwardedRecord>>;
    fn forwarded_by_key(&self, idempotency_key: &str) -> StoreResult<Option<ForwardedRecord>>;
    /// Inserts or replaces the record for `(thread_id, source_id)`.
    fn put_forwarded(&mut self, record: &ForwardedRecord) -> StoreResult<()>;

    /// Drops dedupe and forwarded records past their windows.
    fn prune(&mut self, now_ms: u64) -> StoreResult<()>;
}

/// How long forwarded-command records are kept (the runner's idempotency window, §3.5).
pub const FORWARDED_TTL_MS: u64 = protocol::tunnel::IDEMPOTENCY_KEY_TTL_MS;

/// In-memory [`MachineStore`] for native tests.
#[derive(Debug)]
pub struct MemoryStore {
    state: Option<MachineState>,
    outbox: Vec<OutboxRow>,
    next_id: u64,
    hosted: Vec<(String, HostKind)>,
    events: DedupeWindow,
    forwarded: Vec<ForwardedRecord>,
}

impl Default for MemoryStore {
    fn default() -> Self {
        Self::new()
    }
}

impl MemoryStore {
    pub fn new() -> Self {
        Self {
            state: None,
            outbox: Vec::new(),
            next_id: 0,
            hosted: Vec::new(),
            events: DedupeWindow::event_ids(),
            forwarded: Vec::new(),
        }
    }
}

impl MemoryStore {
    /// Rows in the outbox (tests).
    pub fn outbox_len(&self) -> usize {
        self.outbox.len()
    }
}

impl MachineStore for MemoryStore {
    fn load(&self) -> StoreResult<Option<MachineState>> {
        Ok(self.state.clone())
    }

    fn save(&mut self, state: &MachineState) -> StoreResult<()> {
        self.state = Some(state.clone());
        Ok(())
    }

    fn outbox(&self) -> StoreResult<Vec<OutboxRow>> {
        Ok(self.outbox.clone())
    }

    fn outbox_push(&mut self, row: NewOutbox) -> StoreResult<()> {
        self.next_id += 1;
        self.outbox.push(OutboxRow {
            id: self.next_id,
            thread_id: row.thread_id,
            source_id: row.source_id,
            host: row.host,
            idempotency_key: row.idempotency_key,
            issued_at: row.issued_at,
            enqueued_ms: row.enqueued_ms,
            command: row.command,
        });
        Ok(())
    }

    fn outbox_remove(&mut self, ids: &[u64]) -> StoreResult<()> {
        self.outbox.retain(|row| !ids.contains(&row.id));
        Ok(())
    }

    fn host_thread(&mut self, thread_id: &str, host: HostKind, _now_ms: u64) -> StoreResult<()> {
        match self.hosted.iter_mut().find(|(id, _)| id == thread_id) {
            Some(entry) => entry.1 = host,
            None => self.hosted.push((thread_id.to_owned(), host)),
        }
        Ok(())
    }

    fn hosted(&self, thread_id: &str) -> StoreResult<Option<HostKind>> {
        Ok(self
            .hosted
            .iter()
            .find(|(id, _)| id == thread_id)
            .map(|(_, host)| *host))
    }

    fn hosted_threads(&self) -> StoreResult<Vec<(String, HostKind)>> {
        Ok(self.hosted.clone())
    }

    fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool> {
        // `DedupeWindow::observe` records; probe a copy so only `record_event` records.
        Ok(self.events.clone().observe(event_id, now_ms) == Disposition::Duplicate)
    }

    fn record_event(&mut self, event_id: &str, now_ms: u64) -> StoreResult<()> {
        self.events.observe(event_id, now_ms);
        Ok(())
    }

    fn forwarded(&self, thread_id: &str, source_id: &str) -> StoreResult<Option<ForwardedRecord>> {
        Ok(self
            .forwarded
            .iter()
            .find(|r| r.thread_id == thread_id && r.source_id == source_id)
            .cloned())
    }

    fn forwarded_by_key(&self, idempotency_key: &str) -> StoreResult<Option<ForwardedRecord>> {
        Ok(self
            .forwarded
            .iter()
            .find(|r| r.idempotency_key == idempotency_key)
            .cloned())
    }

    fn put_forwarded(&mut self, record: &ForwardedRecord) -> StoreResult<()> {
        match self
            .forwarded
            .iter_mut()
            .find(|r| r.thread_id == record.thread_id && r.source_id == record.source_id)
        {
            Some(existing) => *existing = record.clone(),
            None => self.forwarded.push(record.clone()),
        }
        Ok(())
    }

    fn prune(&mut self, now_ms: u64) -> StoreResult<()> {
        self.events.prune(now_ms);
        self.forwarded
            .retain(|r| now_ms.saturating_sub(r.updated_ms) < FORWARDED_TTL_MS);
        Ok(())
    }
}
