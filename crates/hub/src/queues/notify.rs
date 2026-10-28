//! The NOTIFY Queue on the Worker (wasm32 only): the producer the Thread DO calls and the
//! consumer `worker.rs` hands each batch to. The logic is in `notify::service`; this file only
//! wires it to the `NOTIFY` binding, D1 and `fetch`.
//!
//! - [`enqueue`]: one message per [`NotifyJob`] (ids only). A failed send is logged and dropped:
//!   a missed push must never fail the append that caused it.
//! - [`consume`]: per message, parse the job, run `process_job` (prefs, send, delete gone
//!   subscriptions) and `retry()` the message when a push service answered 5xx or 429 or a
//!   database call failed; everything else is acknowledged. Within a batch only the last job per
//!   task is sent (`collapse_batch`); across batches the `Topic` header does the same at the push
//!   service. Logs carry counts only, never endpoints, keys or payloads.

use worker::js_sys::Uint8Array;
use worker::wasm_bindgen::JsValue;
use worker::{Env, Fetch, Headers, MessageBatch, MessageExt, Method, Request, RequestInit, Result};

use crate::db::d1::D1;
use crate::notify::env::load_vapid;
use crate::notify::job::NotifyJob;
use crate::notify::provider::{Apns, Fcm, HttpSender, Providers, PushRequest, WebPush};
use crate::notify::service::{collapse_batch, process_job};

/// The Queue producer binding (wrangler.toml `[[queues.producers]]`).
const NOTIFY_BINDING: &str = "NOTIFY";

/// Sends `jobs` to the NOTIFY queue. Never fails the caller.
pub async fn enqueue(env: &Env, jobs: &[NotifyJob]) {
    if jobs.is_empty() {
        return;
    }
    let queue = match env.queue(NOTIFY_BINDING) {
        Ok(queue) => queue,
        Err(error) => {
            worker::console_log!("notify: no NOTIFY queue binding: {error}");
            return;
        }
    };
    for job in jobs {
        if let Err(error) = queue.send(job).await {
            worker::console_log!("notify: enqueue failed: {error}");
        }
    }
}

/// Posts push requests with the Workers `fetch`.
pub struct FetchSender;

impl HttpSender for FetchSender {
    async fn post(&self, request: PushRequest) -> std::result::Result<u16, String> {
        send(request).await.map_err(|error| error.to_string())
    }
}

async fn send(request: PushRequest) -> Result<u16> {
    let headers = Headers::new();
    for (name, value) in &request.headers {
        headers.set(name, value)?;
    }
    let mut init = RequestInit::new();
    init.with_method(Method::Post)
        .with_headers(headers)
        .with_body(Some(JsValue::from(Uint8Array::from(
            request.body.as_slice(),
        ))));
    let outgoing = Request::new_with_init(&request.url, &init)?;
    let response = Fetch::Request(outgoing).send().await?;
    Ok(response.status_code())
}

/// The queue consumer for one batch.
pub async fn consume(batch: MessageBatch<serde_json::Value>, env: &Env) -> Result<()> {
    let messages = batch.messages()?;
    let jobs: Vec<Option<NotifyJob>> = messages
        .iter()
        .map(|message| serde_json::from_value(message.body().clone()).ok())
        .collect();
    let valid: Vec<NotifyJob> = jobs.iter().flatten().cloned().collect();
    let mut keep = collapse_batch(&valid).into_iter();

    let database = env.d1("DB")?;
    let db = D1::new(&database);
    let providers = Providers {
        web: WebPush {
            http: FetchSender,
            vapid: load_vapid(env),
        },
        apns: Apns,
        fcm: Fcm,
    };
    let now_ms = worker::Date::now().as_millis();

    let (mut delivered, mut skipped, mut removed, mut failed, mut retried) = (0, 0, 0, 0, 0);
    for (message, job) in messages.iter().zip(&jobs) {
        let Some(job) = job else {
            worker::console_log!("notify: dropping a malformed job");
            message.ack();
            continue;
        };
        if !keep.next().unwrap_or(true) {
            message.ack();
            continue;
        }
        match process_job(&db, &providers, job, now_ms).await {
            Ok(report) => {
                delivered += report.delivered;
                skipped += report.skipped;
                removed += report.removed;
                failed += report.failed;
                if report.retry {
                    retried += 1;
                    message.retry();
                } else {
                    message.ack();
                }
            }
            Err(error) => {
                worker::console_log!("notify: {error}");
                retried += 1;
                message.retry();
            }
        }
    }
    worker::console_log!(
        "{{\"queue\":\"{}\",\"messages\":{},\"delivered\":{delivered},\"skipped\":{skipped},\"removed\":{removed},\"failed\":{failed},\"retried\":{retried}}}",
        batch.queue(),
        messages.len()
    );
    Ok(())
}
