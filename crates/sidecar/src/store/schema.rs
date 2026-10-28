//! Versioned schema migrations, tracked in `PRAGMA user_version`.

use rusqlite::Connection;

use super::StoreError;

/// One migration per version: `MIGRATIONS[n]` upgrades schema `n` to `n + 1`.
const MIGRATIONS: &[&str] = &[
    // v1
    "
    CREATE TABLE meta (
        key   TEXT PRIMARY KEY NOT NULL,
        value TEXT NOT NULL
    );
    CREATE TABLE threads (
        thread_id  TEXT PRIMARY KEY NOT NULL,
        harness    TEXT NOT NULL,
        cwd        TEXT NOT NULL,
        created_ms INTEGER NOT NULL
    );
    CREATE TABLE tasks (
        task_id    TEXT PRIMARY KEY NOT NULL,
        thread_id  TEXT NOT NULL,
        state      TEXT NOT NULL,
        created_ms INTEGER NOT NULL,
        updated_ms INTEGER NOT NULL
    );
    CREATE INDEX tasks_by_thread ON tasks (thread_id);
    CREATE TABLE sessions (
        thread_id  TEXT PRIMARY KEY NOT NULL,
        harness    TEXT NOT NULL,
        session_id TEXT NOT NULL,
        cwd        TEXT NOT NULL,
        state      TEXT NOT NULL,
        updated_ms INTEGER NOT NULL
    );
    CREATE TABLE outbox (
        seq      INTEGER PRIMARY KEY NOT NULL,
        event_id TEXT NOT NULL UNIQUE,
        body     TEXT NOT NULL
    );
    CREATE TABLE idempotency_keys (
        key     TEXT PRIMARY KEY NOT NULL,
        seen_ms INTEGER NOT NULL
    );
    CREATE TABLE interrupts (
        interrupt_id  TEXT PRIMARY KEY NOT NULL,
        thread_id     TEXT NOT NULL,
        task_id       TEXT NOT NULL,
        kind          TEXT NOT NULL,
        request       TEXT NOT NULL,
        expires_at_ms INTEGER NOT NULL,
        state         TEXT NOT NULL
    );
    ",
];

/// The schema version this build writes.
pub const CURRENT_VERSION: u32 = MIGRATIONS.len() as u32;

/// Brings the database up to [`CURRENT_VERSION`], each step in its own transaction.
pub fn migrate(conn: &mut Connection) -> Result<(), StoreError> {
    let found: u32 = conn.query_row("PRAGMA user_version", [], |row| row.get(0))?;
    if found > CURRENT_VERSION {
        return Err(StoreError::SchemaTooNew {
            found,
            supported: CURRENT_VERSION,
        });
    }
    for (index, sql) in MIGRATIONS.iter().enumerate().skip(found as usize) {
        let tx = conn.transaction()?;
        tx.execute_batch(sql)?;
        // `user_version` cannot be bound as a parameter.
        tx.execute_batch(&format!("PRAGMA user_version = {}", index + 1))?;
        tx.commit()?;
    }
    Ok(())
}
