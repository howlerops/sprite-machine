//! The runner's durable store (SQLite, WAL), under `SM_STATE_DIR`.
//!
//! It holds what must survive a restart or a cold wake (PLAN-v2 §4.1, tunnel.md §4–§5):
//! threads, tasks, the session table (thread → harness, ACP session id, state), the tunnel
//! up-stream outbox with its acked cursor, the resume token, the last processed down-seq,
//! processed idempotency keys (7 days), and interrupts that wait for an answer.
//!
//! Every method is one short transaction. Times come from the injected [`Clock`], so tests
//! control expiry.

mod schema;

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use protocol::tunnel::{Disposition, IDEMPOTENCY_KEY_TTL_MS};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde_json::Value;

pub use schema::CURRENT_VERSION;

/// File name of the database inside the state dir.
pub const DB_FILE: &str = "sidecar.db";

/// Errors from the store.
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    /// SQLite failed.
    #[error("store database error: {0}")]
    Sqlite(#[from] rusqlite::Error),
    /// The state dir or database file could not be prepared.
    #[error("store I/O error at {path}: {source}")]
    Io {
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: std::io::Error,
    },
    /// A stored JSON body could not be encoded or decoded.
    #[error("stored JSON is malformed: {0}")]
    Json(#[from] serde_json::Error),
    /// The database was written by a newer build.
    #[error("database schema v{found} is newer than the supported v{supported}")]
    SchemaTooNew {
        /// The version found on disk.
        found: u32,
        /// The newest version this build knows.
        supported: u32,
    },
    /// A stored value has an unknown shape.
    #[error("corrupt store: {0}")]
    Corrupt(String),
    /// An ack named a seq the outbox never held.
    #[error("ack {seq} is ahead of the last enqueued seq {last}")]
    AckAhead {
        /// The acked seq.
        seq: u64,
        /// The highest seq ever enqueued.
        last: u64,
    },
}

/// A source of the current time in Unix milliseconds.
pub trait Clock: Send + Sync {
    /// Now, in milliseconds since the Unix epoch.
    fn now_ms(&self) -> u64;
}

/// The system clock.
#[derive(Debug, Clone, Copy, Default)]
pub struct SystemClock;

impl Clock for SystemClock {
    fn now_ms(&self) -> u64 {
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
    }
}

/// A clock a test moves by hand.
#[derive(Debug, Default)]
pub struct ManualClock(AtomicU64);

impl ManualClock {
    /// A clock at `ms`.
    pub fn new(ms: u64) -> Self {
        Self(AtomicU64::new(ms))
    }

    /// Moves the clock to `ms`.
    pub fn set(&self, ms: u64) {
        self.0.store(ms, Ordering::SeqCst);
    }

    /// Moves the clock forward by `ms`.
    pub fn advance(&self, ms: u64) {
        self.0.fetch_add(ms, Ordering::SeqCst);
    }
}

impl Clock for ManualClock {
    fn now_ms(&self) -> u64 {
        self.0.load(Ordering::SeqCst)
    }
}

/// A thread the runner serves.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThreadRow {
    /// The thread id.
    pub thread_id: String,
    /// The harness that runs it (for example `claude-code`).
    pub harness: String,
    /// The workspace path.
    pub cwd: String,
    /// When the thread was first stored.
    pub created_ms: u64,
}

/// A task (one prompt turn).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TaskRow {
    /// The task id.
    pub task_id: String,
    /// The thread it belongs to.
    pub thread_id: String,
    /// Free-form state (`running`, `paused`, `done`, `failed`, ...).
    pub state: String,
    /// When the task was first stored.
    pub created_ms: u64,
    /// When the task last changed.
    pub updated_ms: u64,
}

/// The state of a stored ACP session.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SessionState {
    /// Attached to a live agent process.
    Active,
    /// Stored, but the agent process is gone; re-attach before the next prompt.
    Detached,
    /// Finished for good.
    Closed,
}

impl SessionState {
    fn as_str(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Detached => "detached",
            Self::Closed => "closed",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "active" => Ok(Self::Active),
            "detached" => Ok(Self::Detached),
            "closed" => Ok(Self::Closed),
            other => Err(StoreError::Corrupt(format!(
                "unknown session state {other:?}"
            ))),
        }
    }
}

/// One row of the session table: thread → harness and ACP session id.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SessionRow {
    /// The thread id (primary key).
    pub thread_id: String,
    /// The harness the session runs on.
    pub harness: String,
    /// The ACP `sessionId`.
    pub session_id: String,
    /// The working directory passed to `session/new`.
    pub cwd: String,
    /// The session state.
    pub state: SessionState,
    /// When the row last changed.
    pub updated_ms: u64,
}

/// A frame in the up-stream outbox.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxRow {
    /// The tunnel seq.
    pub seq: u64,
    /// The stable `event_id` (the same on every retransmit).
    pub event_id: String,
    /// The frame body.
    pub body: Value,
}

/// What the tunnel client needs to rebuild its `Sender` after a restart.
#[derive(Debug, Clone, PartialEq)]
pub struct OutboxState {
    /// Highest seq the hub acked; everything up to it is pruned.
    pub acked_upto: u64,
    /// The seq the next [`Store::enqueue_outbound`] gets.
    pub next_seq: u64,
    /// Un-acked frames in seq order.
    pub unacked: Vec<OutboxRow>,
}

/// Which ACP request an interrupt came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InterruptKind {
    /// `session/request_permission`.
    Approval,
    /// `elicitation/create`.
    Form,
}

impl InterruptKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Form => "form",
        }
    }

    fn parse(text: &str) -> Result<Self, StoreError> {
        match text {
            "approval" => Ok(Self::Approval),
            "form" => Ok(Self::Form),
            other => Err(StoreError::Corrupt(format!(
                "unknown interrupt kind {other:?}"
            ))),
        }
    }
}

/// An interrupt waiting for an answer.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingInterrupt {
    /// The interrupt id.
    pub interrupt_id: String,
    /// Its thread.
    pub thread_id: String,
    /// Its task.
    pub task_id: String,
    /// `approval` (ACP permission request) or `form` (elicitation).
    pub kind: InterruptKind,
    /// The ACP request that created it, as JSON, so the runner can rebuild its mapper.
    pub request: Value,
    /// Approval TTL deadline, in Unix milliseconds.
    pub expires_at_ms: u64,
}

/// The durable store.
pub struct Store {
    conn: Mutex<Connection>,
    clock: Arc<dyn Clock>,
}

const KEY_RESUME_TOKEN: &str = "resume_token";
const KEY_LAST_DOWN_SEQ: &str = "last_down_seq";
const KEY_ACKED_UPTO: &str = "acked_upto";
const KEY_NEXT_SEQ: &str = "next_seq";

impl Store {
    /// Opens (creating if needed) the store in `state_dir` with the system clock.
    pub fn open(state_dir: &Path) -> Result<Self, StoreError> {
        Self::open_with_clock(state_dir, Arc::new(SystemClock))
    }

    /// Opens the store with an injected clock. The dir is created `0700` and the database
    /// file `0600` on unix.
    pub fn open_with_clock(state_dir: &Path, clock: Arc<dyn Clock>) -> Result<Self, StoreError> {
        prepare_dir(state_dir)?;
        let path = state_dir.join(DB_FILE);
        prepare_file(&path)?;
        let mut conn = Connection::open(&path)?;
        conn.pragma_update(None, "journal_mode", "WAL")?;
        conn.pragma_update(None, "synchronous", "FULL")?;
        conn.busy_timeout(std::time::Duration::from_secs(5))?;
        schema::migrate(&mut conn)?;
        Ok(Self {
            conn: Mutex::new(conn),
            clock,
        })
    }

    /// The injected clock's current time.
    pub fn now_ms(&self) -> u64 {
        self.clock.now_ms()
    }

    /// The schema version on disk.
    pub fn schema_version(&self) -> Result<u32, StoreError> {
        Ok(self
            .conn()
            .query_row("PRAGMA user_version", [], |row| row.get(0))?)
    }

    fn conn(&self) -> MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(PoisonError::into_inner)
    }

    // ---- threads ----------------------------------------------------------------------

    /// Stores a thread. An existing thread keeps its `created_ms`; harness and cwd update.
    pub fn upsert_thread(
        &self,
        thread_id: &str,
        harness: &str,
        cwd: &str,
    ) -> Result<ThreadRow, StoreError> {
        let now = self.now_ms();
        let conn = self.conn();
        conn.execute(
            "INSERT INTO threads (thread_id, harness, cwd, created_ms) VALUES (?1, ?2, ?3, ?4)
             ON CONFLICT(thread_id) DO UPDATE SET harness = ?2, cwd = ?3",
            params![thread_id, harness, cwd, to_i64(now)],
        )?;
        thread_in(&conn, thread_id)?
            .ok_or_else(|| StoreError::Corrupt("thread vanished after upsert".to_owned()))
    }

    /// The thread, if stored.
    pub fn thread(&self, thread_id: &str) -> Result<Option<ThreadRow>, StoreError> {
        thread_in(&self.conn(), thread_id)
    }

    /// Every stored thread, oldest first.
    pub fn threads(&self) -> Result<Vec<ThreadRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT thread_id, harness, cwd, created_ms FROM threads
             ORDER BY created_ms, thread_id",
        )?;
        let rows = stmt.query_map([], thread_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- tasks ------------------------------------------------------------------------

    /// Stores a task or changes its state.
    pub fn upsert_task(
        &self,
        task_id: &str,
        thread_id: &str,
        state: &str,
    ) -> Result<(), StoreError> {
        let now = to_i64(self.now_ms());
        self.conn().execute(
            "INSERT INTO tasks (task_id, thread_id, state, created_ms, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?4)
             ON CONFLICT(task_id) DO UPDATE SET state = ?3, updated_ms = ?4",
            params![task_id, thread_id, state, now],
        )?;
        Ok(())
    }

    /// The task, if stored.
    pub fn task(&self, task_id: &str) -> Result<Option<TaskRow>, StoreError> {
        Ok(self
            .conn()
            .query_row(
                "SELECT task_id, thread_id, state, created_ms, updated_ms FROM tasks
                 WHERE task_id = ?1",
                [task_id],
                task_from_row,
            )
            .optional()?)
    }

    /// The tasks of a thread, oldest first.
    pub fn tasks_for_thread(&self, thread_id: &str) -> Result<Vec<TaskRow>, StoreError> {
        let conn = self.conn();
        let mut stmt = conn.prepare(
            "SELECT task_id, thread_id, state, created_ms, updated_ms FROM tasks
             WHERE thread_id = ?1 ORDER BY created_ms, task_id",
        )?;
        let rows = stmt.query_map([thread_id], task_from_row)?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    // ---- session table ----------------------------------------------------------------

    /// Stores the thread's ACP session.
    pub fn upsert_session(
        &self,
        thread_id: &str,
        harness: &str,
        session_id: &str,
        cwd: &str,
        state: SessionState,
    ) -> Result<(), StoreError> {
        let now = to_i64(self.now_ms());
        self.conn().execute(
            "INSERT INTO sessions (thread_id, harness, session_id, cwd, state, updated_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(thread_id) DO UPDATE SET harness = ?2, session_id = ?3, cwd = ?4,
                 state = ?5, updated_ms = ?6",
            params![thread_id, harness, session_id, cwd, state.as_str(), now],
        )?;
        Ok(())
    }

    /// Sets the state of a thread's session. Returns `false` when the thread has none.
    pub fn set_session_state(
        &self,
        thread_id: &str,
        state: SessionState,
    ) -> Result<bool, StoreError> {
        let now = to_i64(self.now_ms());
        let changed = self.conn().execute(
            "UPDATE sessions SET state = ?2, updated_ms = ?3 WHERE thread_id = ?1",
            params![thread_id, state.as_str(), now],
        )?;
        Ok(changed > 0)
    }

    /// The thread's session row, if any.
    pub fn session_for_thread(&self, thread_id: &str) -> Result<Option<SessionRow>, StoreError> {
        let raw = self
            .conn()
            .query_row(
                "SELECT thread_id, harness, session_id, cwd, state, updated_ms FROM sessions
                 WHERE thread_id = ?1",
                [thread_id],
                session_raw,
            )
            .optional()?;
        raw.map(session_from_raw).transpose()
    }

    /// Every session row, ordered by thread id.
    pub fn sessions(&self) -> Result<Vec<SessionRow>, StoreError> {
        let raws = {
            let conn = self.conn();
            let mut stmt = conn.prepare(
                "SELECT thread_id, harness, session_id, cwd, state, updated_ms FROM sessions
                 ORDER BY thread_id",
            )?;
            let rows = stmt.query_map([], session_raw)?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        raws.into_iter().map(session_from_raw).collect()
    }

    /// Marks every `active` session `detached`. Call at startup: no agent process survives a
    /// restart, so each session must be re-attached before its next prompt.
    pub fn detach_all_sessions(&self) -> Result<usize, StoreError> {
        let now = to_i64(self.now_ms());
        Ok(self.conn().execute(
            "UPDATE sessions SET state = 'detached', updated_ms = ?1 WHERE state = 'active'",
            [now],
        )?)
    }

    // ---- tunnel outbox ----------------------------------------------------------------

    /// Appends a frame body to the up-stream outbox and returns its seq. The same `event_id`
    /// returns the seq it already has, so a retried enqueue never duplicates (tunnel.md §4.2).
    pub fn enqueue_outbound(&self, event_id: &str, body: &Value) -> Result<u64, StoreError> {
        let text = serde_json::to_string(body)?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let existing: Option<i64> = tx
            .query_row(
                "SELECT seq FROM outbox WHERE event_id = ?1",
                [event_id],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(seq) = existing {
            return Ok(from_i64(seq));
        }
        let seq = next_seq_in(&tx)?;
        tx.execute(
            "INSERT INTO outbox (seq, event_id, body) VALUES (?1, ?2, ?3)",
            params![to_i64(seq), event_id, text],
        )?;
        set_meta_in(&tx, KEY_NEXT_SEQ, &(seq + 1).to_string())?;
        tx.commit()?;
        Ok(seq)
    }

    /// Cumulative ack: prunes every frame with `seq ≤ upto` and advances the acked cursor.
    /// A stale ack is a no-op; an ack past the last enqueued seq is an error.
    pub fn ack_outbound(&self, upto: u64) -> Result<(), StoreError> {
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let last = next_seq_in(&tx)? - 1;
        if upto > last {
            return Err(StoreError::AckAhead { seq: upto, last });
        }
        let acked = meta_u64_in(&tx, KEY_ACKED_UPTO)?.unwrap_or(0);
        if upto > acked {
            tx.execute("DELETE FROM outbox WHERE seq <= ?1", [to_i64(upto)])?;
            set_meta_in(&tx, KEY_ACKED_UPTO, &upto.to_string())?;
        }
        tx.commit()?;
        Ok(())
    }

    /// The outbox as it must be restored after a restart: the acked cursor, the next seq, and
    /// the un-acked frames in seq order.
    pub fn outbox_state(&self) -> Result<OutboxState, StoreError> {
        let conn = self.conn();
        let acked_upto = meta_u64_in(&conn, KEY_ACKED_UPTO)?.unwrap_or(0);
        let next_seq = next_seq_in(&conn)?;
        let mut stmt = conn.prepare("SELECT seq, event_id, body FROM outbox ORDER BY seq")?;
        let raws = stmt
            .query_map([], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let unacked = raws
            .into_iter()
            .map(|(seq, event_id, body)| {
                Ok(OutboxRow {
                    seq: from_i64(seq),
                    event_id,
                    body: serde_json::from_str(&body)?,
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        Ok(OutboxState {
            acked_upto,
            next_seq,
            unacked,
        })
    }

    /// Un-acked frames in seq order (what to resend after a reconnect).
    pub fn unacked_outbound(&self) -> Result<Vec<OutboxRow>, StoreError> {
        Ok(self.outbox_state()?.unacked)
    }

    /// Applies a `welcome{resumed:false}`: renumbers the outbox from 1, keeping order and
    /// `event_id`s, and resets the acked cursor (tunnel.md §4.3, mirrors `Sender::on_welcome`).
    pub fn restart_outbound_stream(&self) -> Result<(), StoreError> {
        let state = self.outbox_state()?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        tx.execute("DELETE FROM outbox", [])?;
        let mut count = 0u64;
        for row in &state.unacked {
            count += 1;
            tx.execute(
                "INSERT INTO outbox (seq, event_id, body) VALUES (?1, ?2, ?3)",
                params![
                    to_i64(count),
                    row.event_id,
                    serde_json::to_string(&row.body)?
                ],
            )?;
        }
        set_meta_in(&tx, KEY_ACKED_UPTO, "0")?;
        set_meta_in(&tx, KEY_NEXT_SEQ, &(count + 1).to_string())?;
        tx.commit()?;
        Ok(())
    }

    // ---- resume token and down stream -------------------------------------------------

    /// Persists the tunnel resume token (`None` clears it).
    pub fn set_resume_token(&self, token: Option<&str>) -> Result<(), StoreError> {
        let conn = self.conn();
        match token {
            Some(token) => set_meta_in(&conn, KEY_RESUME_TOKEN, token),
            None => {
                conn.execute("DELETE FROM meta WHERE key = ?1", [KEY_RESUME_TOKEN])?;
                Ok(())
            }
        }
    }

    /// The persisted resume token.
    pub fn resume_token(&self) -> Result<Option<String>, StoreError> {
        meta_in(&self.conn(), KEY_RESUME_TOKEN)
    }

    /// The highest down-stream seq durably processed (`0` when none).
    pub fn last_down_seq(&self) -> Result<u64, StoreError> {
        Ok(meta_u64_in(&self.conn(), KEY_LAST_DOWN_SEQ)?.unwrap_or(0))
    }

    /// Records a processed down-stream frame in one transaction: advances `last_down_seq` to
    /// `seq` and, when the frame carried an idempotency key, observes it. Returns
    /// [`Disposition::Duplicate`] when the key was seen within 7 days (skip the effect, still
    /// ack). The caller acks only after this returns.
    pub fn commit_down_frame(
        &self,
        seq: u64,
        idempotency_key: Option<&str>,
    ) -> Result<Disposition, StoreError> {
        let now = self.now_ms();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let disposition = match idempotency_key {
            Some(key) => observe_key_in(&tx, key, now)?,
            None => Disposition::New,
        };
        let last = meta_u64_in(&tx, KEY_LAST_DOWN_SEQ)?.unwrap_or(0);
        if seq > last {
            set_meta_in(&tx, KEY_LAST_DOWN_SEQ, &seq.to_string())?;
        }
        tx.commit()?;
        Ok(disposition)
    }

    /// Records `key` if new (7-day window); expired keys are pruned first.
    pub fn observe_idempotency_key(&self, key: &str) -> Result<Disposition, StoreError> {
        let now = self.now_ms();
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        let disposition = observe_key_in(&tx, key, now)?;
        tx.commit()?;
        Ok(disposition)
    }

    /// Resets the down-stream cursor for a fresh stream (`welcome{resumed:false}`):
    /// `last_down_seq` goes to 0. Idempotency keys are kept: they dedupe the replay.
    pub fn restart_down_stream(&self) -> Result<(), StoreError> {
        set_meta_in(&self.conn(), KEY_LAST_DOWN_SEQ, "0")
    }

    // ---- interrupts -------------------------------------------------------------------

    /// Stores an interrupt that waits for an answer, with its approval TTL deadline.
    pub fn save_interrupt(&self, interrupt: &PendingInterrupt) -> Result<(), StoreError> {
        self.conn().execute(
            "INSERT INTO interrupts
                 (interrupt_id, thread_id, task_id, kind, request, expires_at_ms, state)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open')
             ON CONFLICT(interrupt_id) DO UPDATE SET thread_id = ?2, task_id = ?3, kind = ?4,
                 request = ?5, expires_at_ms = ?6, state = 'open'",
            params![
                interrupt.interrupt_id,
                interrupt.thread_id,
                interrupt.task_id,
                interrupt.kind.as_str(),
                serde_json::to_string(&interrupt.request)?,
                to_i64(interrupt.expires_at_ms),
            ],
        )?;
        Ok(())
    }

    /// Removes an interrupt that was answered. Returns `false` when it was not open.
    pub fn resolve_interrupt(&self, interrupt_id: &str) -> Result<bool, StoreError> {
        let changed = self.conn().execute(
            "DELETE FROM interrupts WHERE interrupt_id = ?1 AND state = 'open'",
            [interrupt_id],
        )?;
        Ok(changed > 0)
    }

    /// Open interrupts, earliest deadline first.
    pub fn open_interrupts(&self) -> Result<Vec<PendingInterrupt>, StoreError> {
        self.interrupts_where("state = 'open'", None)
    }

    /// Marks every open interrupt whose deadline has passed (by the store's clock) as
    /// expired and returns them, earliest first. Each interrupt is reported once: the runner
    /// ends its task with `RunError{error, code:"expired"}` (see `acp::reattach`).
    pub fn take_expired_interrupts(&self) -> Result<Vec<PendingInterrupt>, StoreError> {
        let now = self.now_ms();
        let expired = self.interrupts_where("state = 'open' AND expires_at_ms <= ?1", Some(now))?;
        let mut conn = self.conn();
        let tx = conn.transaction()?;
        for interrupt in &expired {
            tx.execute(
                "UPDATE interrupts SET state = 'expired' WHERE interrupt_id = ?1",
                [&interrupt.interrupt_id],
            )?;
        }
        tx.commit()?;
        Ok(expired)
    }

    fn interrupts_where(
        &self,
        condition: &str,
        now: Option<u64>,
    ) -> Result<Vec<PendingInterrupt>, StoreError> {
        let sql = format!(
            "SELECT interrupt_id, thread_id, task_id, kind, request, expires_at_ms FROM interrupts
             WHERE {condition} ORDER BY expires_at_ms, interrupt_id"
        );
        let conn = self.conn();
        let mut stmt = conn.prepare(&sql)?;
        let map = |row: &rusqlite::Row<'_>| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, String>(3)?,
                row.get::<_, String>(4)?,
                row.get::<_, i64>(5)?,
            ))
        };
        let raws = match now {
            Some(now) => stmt
                .query_map([to_i64(now)], map)?
                .collect::<Result<Vec<_>, _>>()?,
            None => stmt.query_map([], map)?.collect::<Result<Vec<_>, _>>()?,
        };
        raws.into_iter()
            .map(
                |(interrupt_id, thread_id, task_id, kind, request, expires)| {
                    Ok(PendingInterrupt {
                        interrupt_id,
                        thread_id,
                        task_id,
                        kind: InterruptKind::parse(&kind)?,
                        request: serde_json::from_str(&request)?,
                        expires_at_ms: from_i64(expires),
                    })
                },
            )
            .collect()
    }
}

// ---- helpers --------------------------------------------------------------------------

fn to_i64(value: u64) -> i64 {
    i64::try_from(value).unwrap_or(i64::MAX)
}

fn from_i64(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

fn meta_in(conn: &Connection, key: &str) -> Result<Option<String>, StoreError> {
    Ok(conn
        .query_row("SELECT value FROM meta WHERE key = ?1", [key], |row| {
            row.get(0)
        })
        .optional()?)
}

fn meta_u64_in(conn: &Connection, key: &str) -> Result<Option<u64>, StoreError> {
    meta_in(conn, key)?
        .map(|text| {
            text.parse::<u64>()
                .map_err(|_| StoreError::Corrupt(format!("meta {key} is not a number: {text:?}")))
        })
        .transpose()
}

fn set_meta_in(conn: &Connection, key: &str, value: &str) -> Result<(), StoreError> {
    conn.execute(
        "INSERT INTO meta (key, value) VALUES (?1, ?2)
         ON CONFLICT(key) DO UPDATE SET value = ?2",
        params![key, value],
    )?;
    Ok(())
}

fn next_seq_in(conn: &Connection) -> Result<u64, StoreError> {
    Ok(meta_u64_in(conn, KEY_NEXT_SEQ)?.unwrap_or(1))
}

fn observe_key_in(tx: &Transaction<'_>, key: &str, now: u64) -> Result<Disposition, StoreError> {
    // A key is kept while `now - seen < ttl`, so anything at or past the cutoff is forgotten.
    let cutoff = now.saturating_sub(IDEMPOTENCY_KEY_TTL_MS);
    tx.execute(
        "DELETE FROM idempotency_keys WHERE seen_ms <= ?1 AND ?2 >= ?3",
        params![to_i64(cutoff), to_i64(now), to_i64(IDEMPOTENCY_KEY_TTL_MS)],
    )?;
    let inserted = tx.execute(
        "INSERT OR IGNORE INTO idempotency_keys (key, seen_ms) VALUES (?1, ?2)",
        params![key, to_i64(now)],
    )?;
    Ok(if inserted > 0 {
        Disposition::New
    } else {
        Disposition::Duplicate
    })
}

fn thread_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<ThreadRow> {
    Ok(ThreadRow {
        thread_id: row.get(0)?,
        harness: row.get(1)?,
        cwd: row.get(2)?,
        created_ms: from_i64(row.get(3)?),
    })
}

fn thread_in(conn: &Connection, thread_id: &str) -> Result<Option<ThreadRow>, StoreError> {
    Ok(conn
        .query_row(
            "SELECT thread_id, harness, cwd, created_ms FROM threads WHERE thread_id = ?1",
            [thread_id],
            thread_from_row,
        )
        .optional()?)
}

fn task_from_row(row: &rusqlite::Row<'_>) -> rusqlite::Result<TaskRow> {
    Ok(TaskRow {
        task_id: row.get(0)?,
        thread_id: row.get(1)?,
        state: row.get(2)?,
        created_ms: from_i64(row.get(3)?),
        updated_ms: from_i64(row.get(4)?),
    })
}

type RawSession = (String, String, String, String, String, i64);

fn session_raw(row: &rusqlite::Row<'_>) -> rusqlite::Result<RawSession> {
    Ok((
        row.get(0)?,
        row.get(1)?,
        row.get(2)?,
        row.get(3)?,
        row.get(4)?,
        row.get(5)?,
    ))
}

fn session_from_raw(raw: RawSession) -> Result<SessionRow, StoreError> {
    let (thread_id, harness, session_id, cwd, state, updated) = raw;
    Ok(SessionRow {
        thread_id,
        harness,
        session_id,
        cwd,
        state: SessionState::parse(&state)?,
        updated_ms: from_i64(updated),
    })
}

fn io_error(path: &Path) -> impl FnOnce(std::io::Error) -> StoreError + '_ {
    move |source| StoreError::Io {
        path: path.to_path_buf(),
        source,
    }
}

#[cfg(unix)]
fn prepare_dir(dir: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::{DirBuilderExt, PermissionsExt};
    std::fs::DirBuilder::new()
        .recursive(true)
        .mode(0o700)
        .create(dir)
        .map_err(io_error(dir))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700)).map_err(io_error(dir))
}

#[cfg(not(unix))]
fn prepare_dir(dir: &Path) -> Result<(), StoreError> {
    std::fs::create_dir_all(dir).map_err(io_error(dir))
}

#[cfg(unix)]
fn prepare_file(path: &Path) -> Result<(), StoreError> {
    use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .mode(0o600)
        .open(path)
        .map_err(io_error(path))?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).map_err(io_error(path))
}

#[cfg(not(unix))]
fn prepare_file(path: &Path) -> Result<(), StoreError> {
    std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)
        .map(|_| ())
        .map_err(io_error(path))
}
