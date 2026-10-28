//! [`MachineStore`] over Durable Object SQLite (`ctx.storage().sql()`), wasm32 only.
//!
//! One database per machine. `SqlStorage::exec` is synchronous, so a step that does not
//! `.await` between storage calls is applied atomically (the DO's implicit transaction): the
//! core's "persist the effect and `last` together" rule (tunnel.md §4.2) holds within a step.
//!
//! Tables:
//!
//! - `machine_state (id = 1, json)`: the [`MachineState`] row;
//! - `outbox (id PK, ...)`: the durable down-stream outbox, in `id` order;
//! - `hosted (thread_id PK, host)`: threads this machine hosts;
//! - `event_ids (event_id PK, seen_ms)`: the 24 h `event_id` window;
//! - `forwarded (thread_id, source_id, ...)`: commands the thread hosts pushed.

use protocol::tunnel::{Command, EVENT_ID_TTL_MS};
use serde::Deserialize;
use serde::de::DeserializeOwned;
use serde_json::Value;
use worker::{SqlStorage, SqlStorageValue};

use super::store::{
    FORWARDED_TTL_MS, ForwardedRecord, HostKind, MachineState, MachineStore, NewOutbox, OutboxRow,
    StoreError, StoreResult,
};

const SCHEMA: [&str; 6] = [
    "CREATE TABLE IF NOT EXISTS machine_state (
       id INTEGER PRIMARY KEY CHECK (id = 1), json TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS outbox (
       id INTEGER PRIMARY KEY AUTOINCREMENT, thread_id TEXT NOT NULL, source_id TEXT NOT NULL,
       host TEXT NOT NULL, idempotency_key TEXT NOT NULL, issued_at TEXT NOT NULL,
       enqueued_ms INTEGER NOT NULL, command TEXT NOT NULL)",
    "CREATE TABLE IF NOT EXISTS hosted (
       thread_id TEXT PRIMARY KEY, host TEXT NOT NULL, created_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS event_ids (
       event_id TEXT PRIMARY KEY, seen_ms INTEGER NOT NULL)",
    "CREATE TABLE IF NOT EXISTS forwarded (
       thread_id TEXT NOT NULL, source_id TEXT NOT NULL, idempotency_key TEXT NOT NULL,
       json TEXT NOT NULL, updated_ms INTEGER NOT NULL, PRIMARY KEY (thread_id, source_id))",
    "CREATE INDEX IF NOT EXISTS forwarded_by_key ON forwarded (idempotency_key)",
];

/// SQLite-backed [`MachineStore`].
pub struct SqlMachineStore {
    sql: SqlStorage,
}

#[derive(Deserialize)]
struct JsonRow {
    json: String,
}

#[derive(Deserialize)]
struct OutboxDb {
    id: i64,
    thread_id: String,
    source_id: String,
    host: String,
    idempotency_key: String,
    issued_at: String,
    enqueued_ms: i64,
    command: String,
}

#[derive(Deserialize)]
struct HostedDb {
    thread_id: String,
    host: String,
}

#[derive(Deserialize)]
struct CountDb {
    n: i64,
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

fn unsigned(value: i64) -> u64 {
    u64::try_from(value).unwrap_or(0)
}

impl SqlMachineStore {
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
}

fn host_of(text: &str) -> StoreResult<HostKind> {
    HostKind::parse(text).ok_or_else(|| err(format!("unknown host kind {text}")))
}

impl MachineStore for SqlMachineStore {
    fn load(&self) -> StoreResult<Option<MachineState>> {
        let rows: Vec<JsonRow> =
            self.rows("SELECT json FROM machine_state WHERE id = 1", vec![])?;
        rows.into_iter()
            .next()
            .map(|row| serde_json::from_str(&row.json).map_err(err))
            .transpose()
    }

    fn save(&mut self, state: &MachineState) -> StoreResult<()> {
        let json = serde_json::to_string(state).map_err(err)?;
        self.run(
            "INSERT INTO machine_state (id, json) VALUES (1, ?) \
             ON CONFLICT (id) DO UPDATE SET json = excluded.json",
            vec![SqlStorageValue::String(json)],
        )
    }

    fn outbox(&self) -> StoreResult<Vec<OutboxRow>> {
        let rows: Vec<OutboxDb> = self.rows(
            "SELECT id, thread_id, source_id, host, idempotency_key, issued_at, enqueued_ms, \
             command FROM outbox ORDER BY id",
            vec![],
        )?;
        rows.into_iter()
            .map(|row| {
                Ok(OutboxRow {
                    id: unsigned(row.id),
                    thread_id: row.thread_id,
                    source_id: row.source_id,
                    host: host_of(&row.host)?,
                    idempotency_key: row.idempotency_key,
                    issued_at: row.issued_at,
                    enqueued_ms: unsigned(row.enqueued_ms),
                    command: serde_json::from_str::<Command>(&row.command).map_err(err)?,
                })
            })
            .collect()
    }

    fn outbox_push(&mut self, row: NewOutbox) -> StoreResult<()> {
        let command = serde_json::to_string(&row.command).map_err(err)?;
        self.run(
            "INSERT INTO outbox (thread_id, source_id, host, idempotency_key, issued_at, \
             enqueued_ms, command) VALUES (?, ?, ?, ?, ?, ?, ?)",
            vec![
                text(&row.thread_id),
                text(&row.source_id),
                text(row.host.as_str()),
                text(&row.idempotency_key),
                text(&row.issued_at),
                int(row.enqueued_ms),
                SqlStorageValue::String(command),
            ],
        )
    }

    fn outbox_remove(&mut self, ids: &[u64]) -> StoreResult<()> {
        for id in ids {
            self.run("DELETE FROM outbox WHERE id = ?", vec![int(*id)])?;
        }
        Ok(())
    }

    fn host_thread(&mut self, thread_id: &str, host: HostKind, now_ms: u64) -> StoreResult<()> {
        self.run(
            "INSERT INTO hosted (thread_id, host, created_ms) VALUES (?, ?, ?) \
             ON CONFLICT (thread_id) DO UPDATE SET host = excluded.host",
            vec![text(thread_id), text(host.as_str()), int(now_ms)],
        )
    }

    fn hosted(&self, thread_id: &str) -> StoreResult<Option<HostKind>> {
        let rows: Vec<HostedDb> = self.rows(
            "SELECT thread_id, host FROM hosted WHERE thread_id = ?",
            vec![text(thread_id)],
        )?;
        rows.first().map(|row| host_of(&row.host)).transpose()
    }

    fn hosted_threads(&self) -> StoreResult<Vec<(String, HostKind)>> {
        let rows: Vec<HostedDb> = self.rows(
            "SELECT thread_id, host FROM hosted ORDER BY created_ms, thread_id",
            vec![],
        )?;
        rows.into_iter()
            .map(|row| Ok((row.thread_id, host_of(&row.host)?)))
            .collect()
    }

    fn event_seen(&self, event_id: &str, now_ms: u64) -> StoreResult<bool> {
        let oldest = now_ms.saturating_sub(EVENT_ID_TTL_MS);
        let rows: Vec<CountDb> = self.rows(
            "SELECT COUNT(*) AS n FROM event_ids WHERE event_id = ? AND seen_ms > ?",
            vec![text(event_id), int(oldest)],
        )?;
        Ok(rows.first().is_some_and(|row| row.n > 0))
    }

    fn record_event(&mut self, event_id: &str, now_ms: u64) -> StoreResult<()> {
        self.run(
            "INSERT INTO event_ids (event_id, seen_ms) VALUES (?, ?) \
             ON CONFLICT (event_id) DO UPDATE SET seen_ms = excluded.seen_ms",
            vec![text(event_id), int(now_ms)],
        )
    }

    fn forwarded(&self, thread_id: &str, source_id: &str) -> StoreResult<Option<ForwardedRecord>> {
        let rows: Vec<JsonRow> = self.rows(
            "SELECT json FROM forwarded WHERE thread_id = ? AND source_id = ?",
            vec![text(thread_id), text(source_id)],
        )?;
        rows.into_iter()
            .next()
            .map(|row| serde_json::from_str(&row.json).map_err(err))
            .transpose()
    }

    fn forwarded_by_key(&self, idempotency_key: &str) -> StoreResult<Option<ForwardedRecord>> {
        let rows: Vec<JsonRow> = self.rows(
            "SELECT json FROM forwarded WHERE idempotency_key = ? ORDER BY updated_ms DESC LIMIT 1",
            vec![text(idempotency_key)],
        )?;
        rows.into_iter()
            .next()
            .map(|row| serde_json::from_str(&row.json).map_err(err))
            .transpose()
    }

    fn put_forwarded(&mut self, record: &ForwardedRecord) -> StoreResult<()> {
        let json = serde_json::to_string(record).map_err(err)?;
        self.run(
            "INSERT INTO forwarded (thread_id, source_id, idempotency_key, json, updated_ms) \
             VALUES (?, ?, ?, ?, ?) ON CONFLICT (thread_id, source_id) DO UPDATE SET \
             idempotency_key = excluded.idempotency_key, json = excluded.json, \
             updated_ms = excluded.updated_ms",
            vec![
                text(&record.thread_id),
                text(&record.source_id),
                text(&record.idempotency_key),
                SqlStorageValue::String(json),
                int(record.updated_ms),
            ],
        )
    }

    fn prune(&mut self, now_ms: u64) -> StoreResult<()> {
        self.run(
            "DELETE FROM event_ids WHERE seen_ms <= ?",
            vec![int(now_ms.saturating_sub(EVENT_ID_TTL_MS))],
        )?;
        self.run(
            "DELETE FROM forwarded WHERE updated_ms <= ?",
            vec![int(now_ms.saturating_sub(FORWARDED_TTL_MS))],
        )
    }
}
