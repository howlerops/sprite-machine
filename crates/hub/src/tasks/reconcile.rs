//! The Cron Trigger's reconcile of the task index (M2-010a, wasm32 only).
//!
//! The index is written by the Thread DO after the requests that change a thread (M2-010). Two
//! things can leave a thread without a row or with an old one: a thread that existed before
//! migration 0008, and a D1 upsert that failed (it is only logged). Each hourly pass takes the
//! hosted threads with no row first, then the ones changed most recently, up to
//! [`store::RECONCILE_LIMIT`], and asks each Thread DO to reindex (`POST
//! /internal/threads/{id}/reindex`): the DO builds its row and writes it when the stored row is
//! missing or differs (`sync::reconcile_thread`). Older, unchanged threads past the limit are
//! reached as the newest ones settle.

use serde_json::Value;
use worker::{Env, Method};

use super::store;
use crate::db::d1::D1;
use crate::machines::internal::call;

/// Reconciles one batch. Failures are logged; the next pass tries again.
pub async fn run(env: &Env) {
    let result = async {
        let database = env
            .d1("DB")
            .map_err(|error| format!("D1 binding DB: {error}"))?;
        store::reconcile_candidates(&D1::new(&database), store::RECONCILE_LIMIT)
            .await
            .map_err(|error| error.to_string())
    }
    .await;
    let candidates = match result {
        Ok(candidates) => candidates,
        Err(message) => {
            worker::console_log!("task index reconcile failed: {message}");
            return;
        }
    };
    let mut failed = 0usize;
    for (_org_id, thread_id) in &candidates {
        let path = format!("/internal/threads/{thread_id}/reindex");
        let none: Option<&Value> = None;
        match call(env, "THREAD", thread_id, Method::Post, &path, none).await {
            Ok((status, _)) if status == 204 || status == 404 => {}
            Ok((status, _)) => {
                failed += 1;
                worker::console_log!("task index reconcile: thread {thread_id}: HTTP {status}");
            }
            Err(error) => {
                failed += 1;
                worker::console_log!("task index reconcile: thread {thread_id}: {error}");
            }
        }
    }
    worker::console_log!(
        "task index reconcile: {} threads, {failed} failed",
        candidates.len()
    );
}
