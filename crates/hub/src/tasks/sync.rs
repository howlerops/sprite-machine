//! The Thread DO's half of the task index (M2-010, wasm32 only): writes the thread's row in D1.
//! The org is the one that owns the thread's host row (`thread_hosts`, migration 0006); a thread
//! with no host row has no org and is not indexed.

use worker::Env;

use super::index::IndexRow;
use super::store;
use crate::db::d1::D1;
use crate::db::queries;

/// Upserts the thread's index row. Called by the Thread DO after a request that changed the
/// title, status or lifecycle (`tasks::index::changes_index`). An error is returned to the
/// caller to log: the event is already stored, and the next change upserts the row again.
pub async fn upsert_thread(env: &Env, row: &IndexRow) -> Result<(), String> {
    let database = env
        .d1("DB")
        .map_err(|error| format!("D1 binding DB: {error}"))?;
    let db = D1::new(&database);
    let Some(org_id) = queries::thread_org(&db, &row.thread_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(());
    };
    store::upsert(&db, &org_id, row)
        .await
        .map_err(|error| error.to_string())
}

/// The cron reconcile's write (`tasks::reconcile`): upserts the row when the thread has none or
/// the stored one differs from the Thread DO's view, and leaves a current row alone, so a pass
/// does not move `updated_at` of threads that are in step. Returns `true` when it wrote.
pub async fn reconcile_thread(env: &Env, row: &IndexRow) -> Result<bool, String> {
    let database = env
        .d1("DB")
        .map_err(|error| format!("D1 binding DB: {error}"))?;
    let db = D1::new(&database);
    let Some(org_id) = queries::thread_org(&db, &row.thread_id)
        .await
        .map_err(|error| error.to_string())?
    else {
        return Ok(false);
    };
    let stored = store::stored(&db, &org_id, &row.thread_id)
        .await
        .map_err(|error| error.to_string())?;
    if stored.is_some_and(|stored| stored.is_current(row)) {
        return Ok(false);
    }
    store::upsert(&db, &org_id, row)
        .await
        .map_err(|error| error.to_string())?;
    Ok(true)
}
