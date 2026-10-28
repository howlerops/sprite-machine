//! [`ThreadStore`] over Durable Object SQLite (`ctx.storage().sql()`), wasm32 only.
//!
//! One database per thread. `SqlStorage::exec` is synchronous, so a request that does not
//! `.await` between storage calls is applied atomically (the DO's implicit transaction).
//!
//! Tables (records are JSON in a `json` column next to the columns used for lookups):
//!
//! - `events (seq PK, json)`: the append-only log;
//! - `runs`, `states (scope, seq)`, `tasks (ord, task_id, status)`, `interrupts`, `commands`
//!   (the outbox), `idempotency (user_id, key)`, `event_ids (event_id, seen_ms)` (24 h),
//!   `meta (key)`.

use protocol::tunnel::Command;
use protocol::turn_event::{Envelope, StateScope};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use worker::{SqlStorage, SqlStorageValue};

use super::core::{EVENT_ID_TTL_MS, scope_name};
use super::store::{
    CommandRecord, IdempotencyRecord, InterruptRecord, RunRecord, StoreError, StoreResult,
    TaskRecord, TaskStatus, ThreadStore,
};

const SCHEMA: [&str; 9] = [
    "CREATE TABLE IF NOT EXISTS events (seq INTEGER PRIMARY KEY, json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS runs (
       run_id TEXT PRIMARY KEY, start_seq INTEGER NOT NULL, end_seq INTEGER, json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS states (
       scope TEXT NOT NULL, seq INTEGER NOT NULL, json TEXT NOT NULL, PRIMARY KEY (scope, seq))",
    "CREATE TABLE IF NOT EXISTS tasks (
       ord INTEGER PRIMARY KEY AUTOINCREMENT, task_id TEXT NOT NULL UNIQUE,
       status TEXT NOT NULL, json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS interrupts (
       interrupt_id TEXT PRIMARY KEY, task_id TEXT NOT NULL, open INTEGER NOT NULL,
       json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS commands (
       id INTEGER PRIMARY KEY AUTOINCREMENT, kind TEXT NOT NULL, task_id TEXT,
       acked INTEGER NOT NULL DEFAULT 0, json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS idempotency (
       user_id TEXT NOT NULL, key TEXT NOT NULL, created_ms INTEGER NOT NULL,
       json TEXT NOT NULL, PRIMARY KEY (user_id, key))",
    "CREATE TABLE IF NOT EXISTS event_ids (
       event_id TEXT PRIMARY KEY, seen_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS meta (key TEXT PRIMARY KEY, json TEXT NOT NULL)",
];

/// SQLite-backed [`ThreadStore`]. Cloning it clones the handle to the same database (the catch-up
/// iterator reads pages through its own handle, see `ThreadCore::catch_up`).
#[derive(Clone)]
pub struct SqlStore {
    sql: SqlStorage,
}

#[derive(Deserialize)]
struct JsonRow {
    json: String,
}

#[derive(Deserialize)]
struct HeadRow {
    head: i64,
}

#[derive(Deserialize)]
struct BoundaryRow {
    boundary: Option<i64>,
}

fn err(error: impl std::fmt::Display) -> StoreError {
    StoreError::new(error)
}

fn int(value: u64) -> SqlStorageValue {
    SqlStorageValue::Integer(i64::try_from(value).unwrap_or(i64::MAX))
}

fn text(value: &str) -> SqlStorageValue {
    SqlStorageValue::String(value.to_owned())
}

fn to_json<T: serde::Serialize>(value: &T) -> StoreResult<SqlStorageValue> {
    serde_json::to_string(value)
        .map(SqlStorageValue::String)
        .map_err(err)
}

fn from_json<T: DeserializeOwned>(row: JsonRow) -> StoreResult<T> {
    serde_json::from_str(&row.json).map_err(err)
}

impl SqlStore {
    /// Opens the store and creates the schema if needed.
    pub fn open(sql: SqlStorage) -> StoreResult<Self> {
        let store = Self { sql };
        for statement in SCHEMA {
            store.run(statement, Vec::new())?;
        }
        Ok(store)
    }

    fn run(&self, query: &str, bindings: Vec<SqlStorageValue>) -> StoreResult<()> {
        // Draining the cursor guarantees the statement ran to completion.
        self.sql
            .exec(query, Some(bindings))
            .map_err(err)?
            .to_array::<Value>()
            .map(drop)
            .map_err(err)
    }

    fn rows<T: DeserializeOwned>(
        &self,
        query: &str,
        bindings: Vec<SqlStorageValue>,
    ) -> StoreResult<Vec<T>> {
        self.sql
            .exec(query, Some(bindings))
            .map_err(err)?
            .to_array()
            .map_err(err)
    }

    fn one_json<T: DeserializeOwned>(
        &self,
        query: &str,
        bindings: Vec<SqlStorageValue>,
    ) -> StoreResult<Option<T>> {
        self.rows::<JsonRow>(query, bindings)?
            .into_iter()
            .next()
            .map(from_json)
            .transpose()
    }
}

impl ThreadStore for SqlStore {
    fn head_seq(&self) -> StoreResult<u64> {
        let rows: Vec<HeadRow> = self.rows(
            "SELECT COALESCE(MAX(seq), 0) AS head FROM events",
            Vec::new(),
        )?;
        Ok(rows
            .first()
            .map_or(0, |row| u64::try_from(row.head).unwrap_or(0)))
    }

    fn append_event(&mut self, envelope: &Envelope) -> StoreResult<()> {
        self.run(
            "INSERT INTO events (seq, json) VALUES (?, ?)",
            vec![int(envelope.seq), to_json(envelope)?],
        )
    }

    fn read_range(&self, from: u64, to: u64) -> StoreResult<Vec<Envelope>> {
        self.rows::<JsonRow>(
            "SELECT json FROM events WHERE seq >= ? AND seq <= ? ORDER BY seq",
            vec![int(from), int(to)],
        )?
        .into_iter()
        .map(from_json)
        .collect()
    }

    fn put_run(&mut self, run: &RunRecord) -> StoreResult<()> {
        let end = run.end_seq.map_or(SqlStorageValue::Null, int);
        self.run(
            "INSERT INTO runs (run_id, start_seq, end_seq, json) VALUES (?, ?, ?, ?)
             ON CONFLICT (run_id) DO UPDATE SET end_seq = excluded.end_seq, json = excluded.json",
            vec![text(&run.run_id), int(run.start_seq), end, to_json(run)?],
        )
    }

    fn get_run(&self, run_id: &str) -> StoreResult<Option<RunRecord>> {
        self.one_json("SELECT json FROM runs WHERE run_id = ?", vec![text(run_id)])
    }

    fn run_boundary_at(&self, after: u64) -> StoreResult<Option<u64>> {
        let rows: Vec<BoundaryRow> = self.rows(
            "SELECT MIN(start_seq) AS boundary FROM runs
             WHERE start_seq <= ? AND (end_seq IS NULL OR end_seq > ?)",
            vec![int(after), int(after)],
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.boundary)
            .and_then(|value| u64::try_from(value).ok()))
    }

    fn run_start_back(&self, back: u64) -> StoreResult<Option<u64>> {
        let rows: Vec<BoundaryRow> = self.rows(
            "SELECT start_seq AS boundary FROM runs ORDER BY start_seq DESC LIMIT 1 OFFSET ?",
            vec![int(back)],
        )?;
        Ok(rows
            .first()
            .and_then(|row| row.boundary)
            .and_then(|value| u64::try_from(value).ok()))
    }

    fn put_state(&mut self, scope: StateScope, seq: u64, state: &Value) -> StoreResult<()> {
        self.run(
            "INSERT OR REPLACE INTO states (scope, seq, json) VALUES (?, ?, ?)",
            vec![text(scope_name(scope)), int(seq), to_json(state)?],
        )
    }

    fn state_at(&self, scope: StateScope, seq: u64) -> StoreResult<Option<Value>> {
        self.one_json(
            "SELECT json FROM states WHERE scope = ? AND seq <= ? ORDER BY seq DESC LIMIT 1",
            vec![text(scope_name(scope)), int(seq)],
        )
    }

    fn compact_states(&mut self, scope: StateScope, floor: u64) -> StoreResult<()> {
        self.run(
            "DELETE FROM states WHERE scope = ? AND seq < (
               SELECT COALESCE(MAX(seq), 0) FROM states WHERE scope = ? AND seq <= ?)",
            vec![text(scope_name(scope)), text(scope_name(scope)), int(floor)],
        )
    }

    fn put_task(&mut self, task: &TaskRecord) -> StoreResult<()> {
        self.run(
            "INSERT INTO tasks (task_id, status, json) VALUES (?, ?, ?)
             ON CONFLICT (task_id) DO UPDATE SET status = excluded.status, json = excluded.json",
            vec![
                text(&task.task_id),
                text(task.status.as_str()),
                to_json(task)?,
            ],
        )
    }

    fn get_task(&self, task_id: &str) -> StoreResult<Option<TaskRecord>> {
        self.one_json(
            "SELECT json FROM tasks WHERE task_id = ?",
            vec![text(task_id)],
        )
    }

    fn tasks_with_status(&self, status: TaskStatus) -> StoreResult<Vec<TaskRecord>> {
        self.rows::<JsonRow>(
            "SELECT json FROM tasks WHERE status = ? ORDER BY ord",
            vec![text(status.as_str())],
        )?
        .into_iter()
        .map(from_json)
        .collect()
    }

    fn put_interrupt(&mut self, interrupt: &InterruptRecord) -> StoreResult<()> {
        self.run(
            "INSERT INTO interrupts (interrupt_id, task_id, open, json) VALUES (?, ?, ?, ?)
             ON CONFLICT (interrupt_id) DO UPDATE SET open = excluded.open, json = excluded.json",
            vec![
                text(&interrupt.interrupt_id),
                text(&interrupt.task_id),
                int(u64::from(interrupt.resolved.is_none())),
                to_json(interrupt)?,
            ],
        )
    }

    fn get_interrupt(&self, interrupt_id: &str) -> StoreResult<Option<InterruptRecord>> {
        self.one_json(
            "SELECT json FROM interrupts WHERE interrupt_id = ?",
            vec![text(interrupt_id)],
        )
    }

    fn open_interrupts(&self, task_id: &str) -> StoreResult<Vec<InterruptRecord>> {
        self.rows::<JsonRow>(
            "SELECT json FROM interrupts WHERE task_id = ? AND open = 1 ORDER BY rowid",
            vec![text(task_id)],
        )?
        .into_iter()
        .map(from_json)
        .collect()
    }

    fn queue_command(
        &mut self,
        idempotency_key: &str,
        issued_at: &str,
        command: &Command,
    ) -> StoreResult<u64> {
        let (kind, task_id) = match command {
            Command::Prompt { task_id, .. } => ("prompt", Some(task_id.as_str())),
            Command::Approval { task_id, .. } => ("approval", Some(task_id.as_str())),
            Command::Dispatch { task_id, .. } => ("dispatch", Some(task_id.as_str())),
            Command::Cancel { task_id, .. } => ("cancel", Some(task_id.as_str())),
            Command::SessionConfig { .. } => ("session_config", None),
        };
        // The id is assigned by the table; the JSON carries everything but the id.
        let record = serde_json::json!({
            "idempotency_key": idempotency_key,
            "issued_at": issued_at,
            "command": command,
        });
        #[derive(Deserialize)]
        struct IdRow {
            id: i64,
        }
        let rows: Vec<IdRow> = self.rows(
            "INSERT INTO commands (kind, task_id, json) VALUES (?, ?, ?) RETURNING id",
            vec![
                text(kind),
                task_id.map_or(SqlStorageValue::Null, text),
                to_json(&record)?,
            ],
        )?;
        rows.first()
            .and_then(|row| u64::try_from(row.id).ok())
            .ok_or_else(|| err("INSERT INTO commands returned no id"))
    }

    fn unacked_commands(&self, limit: usize) -> StoreResult<Vec<CommandRecord>> {
        #[derive(Deserialize)]
        struct CommandRow {
            id: i64,
            json: String,
        }
        #[derive(Deserialize)]
        struct Stored {
            idempotency_key: String,
            issued_at: String,
            command: Command,
        }
        self.rows::<CommandRow>(
            "SELECT id, json FROM commands WHERE acked = 0 ORDER BY id LIMIT ?",
            vec![int(limit as u64)],
        )?
        .into_iter()
        .map(|row| {
            let stored: Stored = serde_json::from_str(&row.json).map_err(err)?;
            Ok(CommandRecord {
                id: u64::try_from(row.id).map_err(err)?,
                idempotency_key: stored.idempotency_key,
                issued_at: stored.issued_at,
                command: stored.command,
            })
        })
        .collect()
    }

    fn ack_commands(&mut self, ids: &[u64]) -> StoreResult<usize> {
        let mut changed = 0;
        for id in ids {
            let rows: Vec<Value> = self.rows(
                "UPDATE commands SET acked = 1 WHERE id = ? AND acked = 0 RETURNING 1 AS n",
                vec![int(*id)],
            )?;
            changed += rows.len();
        }
        Ok(changed)
    }

    fn retract_prompt_command(&mut self, task_id: &str) -> StoreResult<bool> {
        let rows: Vec<Value> = self.rows(
            "DELETE FROM commands WHERE kind IN ('prompt', 'dispatch') AND task_id = ? AND acked = 0
             RETURNING 1 AS n",
            vec![text(task_id)],
        )?;
        Ok(!rows.is_empty())
    }

    fn get_idempotency(&self, user_id: &str, key: &str) -> StoreResult<Option<IdempotencyRecord>> {
        self.one_json(
            "SELECT json FROM idempotency WHERE user_id = ? AND key = ?",
            vec![text(user_id), text(key)],
        )
    }

    fn put_idempotency(&mut self, record: &IdempotencyRecord) -> StoreResult<()> {
        self.run(
            "INSERT OR REPLACE INTO idempotency (user_id, key, created_ms, json)
             VALUES (?, ?, ?, ?)",
            vec![
                text(&record.user_id),
                text(&record.key),
                int(record.created_ms),
                to_json(record)?,
            ],
        )
    }

    fn prune_idempotency(&mut self, older_than_ms: u64) -> StoreResult<()> {
        self.run(
            "DELETE FROM idempotency WHERE created_ms < ?",
            vec![int(older_than_ms)],
        )
    }

    fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool> {
        let rows: Vec<Value> = self.rows(
            "SELECT 1 AS n FROM event_ids WHERE event_id = ? AND seen_ms + ? > ?",
            vec![text(event_id), int(EVENT_ID_TTL_MS), int(now_ms)],
        )?;
        Ok(!rows.is_empty())
    }

    fn record_event(&mut self, event_id: &str, now_ms: u64) -> StoreResult<()> {
        self.run(
            "DELETE FROM event_ids WHERE seen_ms + ? <= ?",
            vec![int(EVENT_ID_TTL_MS), int(now_ms)],
        )?;
        self.run(
            "INSERT OR REPLACE INTO event_ids (event_id, seen_ms) VALUES (?, ?)",
            vec![text(event_id), int(now_ms)],
        )
    }

    fn get_meta(&self, key: &str) -> StoreResult<Option<Value>> {
        self.one_json("SELECT json FROM meta WHERE key = ?", vec![text(key)])
    }

    fn put_meta(&mut self, key: &str, value: &Value) -> StoreResult<()> {
        self.run(
            "INSERT OR REPLACE INTO meta (key, json) VALUES (?, ?)",
            vec![text(key), to_json(value)?],
        )
    }

    fn next_counter(&mut self) -> StoreResult<u64> {
        #[derive(Deserialize)]
        struct ValueRow {
            json: String,
        }
        let rows: Vec<ValueRow> = self.rows(
            "INSERT INTO meta (key, json) VALUES ('counter', '1')
             ON CONFLICT (key) DO UPDATE SET json = CAST(CAST(json AS INTEGER) + 1 AS TEXT)
             RETURNING json",
            Vec::new(),
        )?;
        rows.first()
            .and_then(|row| row.json.parse().ok())
            .ok_or_else(|| err("counter update returned no value"))
    }
}
