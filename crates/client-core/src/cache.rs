//! SQLite cache (rusqlite, bundled): the stored TurnEvent log per thread, and the persisted
//! outbound queue. A restart reads [`Cache::last_seq`] and resubscribes with `?after=` that
//! value, so the client never has to re-fetch what it already holds.

use std::path::Path;

use protocol::turn_event::Envelope;
use rusqlite::{Connection, OptionalExtension, params};

/// A cache failure.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("sqlite: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("stored event is not valid JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("thread {thread_id}: event seq {got} does not follow stored seq {last}")]
    Gap {
        thread_id: String,
        last: u64,
        got: u64,
    },
    #[error("envelope for thread {got} written to thread {thread_id}")]
    WrongThread { thread_id: String, got: String },
}

/// The client's local database.
pub struct Cache {
    conn: Connection,
}

impl Cache {
    /// Opens (creating if needed) the database file at `path`.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, CacheError> {
        Self::from_connection(Connection::open(path)?)
    }

    /// A private in-memory database (tests, and the headless tracer).
    pub fn open_in_memory() -> Result<Self, CacheError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    fn from_connection(conn: Connection) -> Result<Self, CacheError> {
        conn.execute_batch(
            "CREATE TABLE IF NOT EXISTS events (
                thread_id TEXT    NOT NULL,
                seq       INTEGER NOT NULL,
                envelope  TEXT    NOT NULL,
                PRIMARY KEY (thread_id, seq)
            );
            CREATE TABLE IF NOT EXISTS outbox (
                id     INTEGER PRIMARY KEY AUTOINCREMENT,
                record TEXT NOT NULL
            );",
        )?;
        Ok(Self { conn })
    }

    /// The highest stored `seq` of `thread_id`, or `0` when nothing is stored.
    pub fn last_seq(&self, thread_id: &str) -> Result<u64, CacheError> {
        let last: i64 = self.conn.query_row(
            "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
            params![thread_id],
            |row| row.get(0),
        )?;
        Ok(u64::try_from(last).unwrap_or(0))
    }

    /// Stores `envelopes` of `thread_id` in one transaction. Envelopes at or below the stored
    /// seq are duplicates and are skipped; a seq that skips ahead is an error. Returns the new
    /// last seq.
    pub fn append_events(
        &mut self,
        thread_id: &str,
        envelopes: &[Envelope],
    ) -> Result<u64, CacheError> {
        let tx = self.conn.transaction()?;
        let mut last = {
            let stored: i64 = tx.query_row(
                "SELECT COALESCE(MAX(seq), 0) FROM events WHERE thread_id = ?1",
                params![thread_id],
                |row| row.get(0),
            )?;
            u64::try_from(stored).unwrap_or(0)
        };
        for envelope in envelopes {
            if envelope.thread_id != thread_id {
                return Err(CacheError::WrongThread {
                    thread_id: thread_id.to_owned(),
                    got: envelope.thread_id.clone(),
                });
            }
            if envelope.seq <= last {
                continue;
            }
            if envelope.seq != last + 1 {
                return Err(CacheError::Gap {
                    thread_id: thread_id.to_owned(),
                    last,
                    got: envelope.seq,
                });
            }
            let json = serde_json::to_string(envelope)?;
            let seq = i64::try_from(envelope.seq).unwrap_or(i64::MAX);
            tx.execute(
                "INSERT INTO events (thread_id, seq, envelope) VALUES (?1, ?2, ?3)",
                params![thread_id, seq, json],
            )?;
            last = envelope.seq;
        }
        tx.commit()?;
        Ok(last)
    }

    /// Every stored envelope of `thread_id`, in `seq` order.
    pub fn events(&self, thread_id: &str) -> Result<Vec<Envelope>, CacheError> {
        let mut statement = self
            .conn
            .prepare("SELECT envelope FROM events WHERE thread_id = ?1 ORDER BY seq")?;
        let rows = statement.query_map(params![thread_id], |row| row.get::<_, String>(0))?;
        let mut envelopes = Vec::new();
        for row in rows {
            envelopes.push(serde_json::from_str(&row?)?);
        }
        Ok(envelopes)
    }

    /// Inserts an outbox record and returns its local id.
    pub fn insert_outbox(&self, record: &str) -> Result<i64, CacheError> {
        self.conn
            .execute("INSERT INTO outbox (record) VALUES (?1)", params![record])?;
        Ok(self.conn.last_insert_rowid())
    }

    /// Replaces the outbox record `id`.
    pub fn update_outbox(&self, id: i64, record: &str) -> Result<(), CacheError> {
        self.conn.execute(
            "UPDATE outbox SET record = ?2 WHERE id = ?1",
            params![id, record],
        )?;
        Ok(())
    }

    /// Removes the outbox record `id`.
    pub fn delete_outbox(&self, id: i64) -> Result<(), CacheError> {
        self.conn
            .execute("DELETE FROM outbox WHERE id = ?1", params![id])?;
        Ok(())
    }

    /// Every outbox record as `(id, record)`, in id order.
    pub fn outbox_rows(&self) -> Result<Vec<(i64, String)>, CacheError> {
        let mut statement = self
            .conn
            .prepare("SELECT id, record FROM outbox ORDER BY id")?;
        let rows = statement.query_map([], |row| Ok((row.get(0)?, row.get(1)?)))?;
        Ok(rows.collect::<Result<_, _>>()?)
    }

    /// The stored record for one outbox id, if any.
    pub fn outbox_row(&self, id: i64) -> Result<Option<String>, CacheError> {
        Ok(self
            .conn
            .query_row(
                "SELECT record FROM outbox WHERE id = ?1",
                params![id],
                |row| row.get(0),
            )
            .optional()?)
    }
}
